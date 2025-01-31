#![feature(box_syntax, box_patterns)]

extern crate rlp;
extern crate sha3;

pub mod utils;

use sha3::{Digest, Keccak256};
use utils::*;

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Hash(Vec<u8>, usize), // (Hash, # empty spaces)
    Leaf(NibbleKey, Vec<u8>),
    Extension(Vec<u8>, Box<Node>),
    FullNode(Vec<Node>),
    EmptySlot,
}

impl rlp::Encodable for Node {
    fn rlp_append(&self, s: &mut rlp::RlpStream) {
        match self {
            Node::Leaf(ref k, ref v) => {
                s.begin_list(2).append(k).append(v);
            }
            _ => panic!("Not supported yet!"),
        }
    }
}

impl rlp::Decodable for Node {
    fn decode(rlp: &rlp::Rlp) -> Result<Self, rlp::DecoderError> {
        if !rlp.is_list() {
            return Err(rlp::DecoderError::RlpExpectedToBeList);
        }
        let keyval = rlp.as_list::<Vec<u8>>()?;
        Ok(Node::Leaf(
            NibbleKey::new(keyval[0].clone()),
            keyval[1].clone(),
        ))
    }
}

impl Node {
    fn hash(&self, hashers: &mut Vec<Keccak256>) -> Vec<u8> {
        use Node::*;
        match self {
            EmptySlot => Vec::new(),
            Leaf(_, _) => {
                let encoding = rlp::encode(self);

                // Only hash if the encoder output is less than 32 bytes.
                if encoding.len() > 32 {
                    let mut hasher = Keccak256::new();
                    hasher.input(&encoding);
                    Vec::<u8>::from(&hasher.result()[..])
                } else {
                    encoding
                }
            }
            Extension(ref ext, node) => {
                let subtree_hash = node.hash(hashers);
                let encoding =
                    rlp::encode_list::<Vec<u8>, Vec<u8>>(&vec![ext.clone(), subtree_hash.clone()]);

                // Only hash if the encoder output is less than 32 bytes.
                if encoding.len() > 32 {
                    let mut hasher = Keccak256::new();
                    hasher.input(&encoding);
                    Vec::<u8>::from(&hasher.result()[..])
                } else {
                    encoding
                }
            }
            FullNode(ref nodes) => {
                let mut keys = Vec::new();
                for node in nodes {
                    keys.push(node.hash(hashers));
                }
                let encoding = rlp::encode_list::<Vec<u8>, Vec<u8>>(&keys[..]);

                // Only hash if the encoder output is less than 32 bytes.
                if encoding.len() > 32 {
                    let mut hasher = Keccak256::new();
                    hasher.input(&encoding);
                    Vec::<u8>::from(&hasher.result()[..])
                } else {
                    encoding
                }
            }
            Hash(h, _) => h.to_vec(),
        }
    }
}

#[derive(Debug)]
pub enum Instruction {
    BRANCH(usize),
    HASHER(usize),
    LEAF(usize),
    EXTENSION(Vec<u8>),
    ADD(usize),
}

#[derive(Debug)]
pub struct Multiproof {
    pub hashes: Vec<Vec<u8>>,           // List of hashes in the proof
    pub instructions: Vec<Instruction>, // List of instructions in the proof
    pub keyvals: Vec<Vec<u8>>,          // List of RLP-encoded (key, value) pairs in the proof
}

// Rebuilds the tree based on the multiproof components
pub fn rebuild(stack: &mut Vec<Node>, proof: &Multiproof) -> Node {
    use Instruction::*;
    use Node::*;

    let mut hiter = proof.hashes.iter();
    let iiter = proof.instructions.iter();
    let mut kviter = proof.keyvals.iter().map(|encoded| {
        // Deserialize the keys as they are read
        rlp::decode::<Node>(encoded).unwrap()
    });

    for instr in iiter {
        match instr {
            HASHER(digit) => {
                if let Some(h) = hiter.next() {
                    stack.push(Hash(h.to_vec(), *digit));
                } else {
                    panic!("Proof requires one more hash in HASHER")
                }
            }
            LEAF(keylength) => {
                if let Some(Leaf(key, value)) = kviter.next() {
                    stack.push(Leaf(key.keep_suffix(*keylength), value.to_vec()));
                } else {
                    panic!("Proof requires one more (key,value) pair in LEAF");
                }
            }
            BRANCH(digit) => {
                if let Some(node) = stack.pop() {
                    let mut children = vec![Node::EmptySlot; 16];
                    children[*digit] = node;
                    stack.push(FullNode(children))
                } else {
                    panic!("Could not pop a value from the stack, that is required for a BRANCH")
                }
            }
            EXTENSION(key) => {
                if let Some(node) = stack.pop() {
                    stack.push(Extension(key.to_vec(), Box::new(node)));
                } else {
                    panic!("Could not find a node on the stack, that is required for an EXTENSION")
                }
            }
            ADD(digit) => {
                if let (Some(el1), Some(el2)) = (stack.pop(), stack.last_mut()) {
                    match el2 {
                        FullNode(ref mut n2) => {
                            if *digit >= n2.len() {
                                panic!(format!(
                                    "Incorrect full node index: {} > {}",
                                    digit,
                                    n2.len() - 1
                                ))
                            }

                            // A hash needs to be fed into the hash sponge, any other node is simply
                            // a child (el1) of the parent node (el2). this is done during resolve.
                            n2[*digit] = el1;
                        }
                        Hash(_, _) => panic!("Hash node no longer supported in this case"),
                        _ => panic!("Unexpected node type"),
                    }
                } else {
                    panic!("Could not find enough parameters to ADD")
                }
            }
        }
    }

    stack.pop().unwrap()
}

// Utility function to find the length of the common prefix of two keys
fn find_common_length(s1: &[u8], s2: &[u8]) -> usize {
    let (longuest, shortest) = if s1.len() > s2.len() {
        (s1, s2)
    } else {
        (s2, s1)
    };
    let mut firstdiffindex = shortest.len();
    for (i, &n) in shortest.iter().enumerate() {
        if n != longuest[i] {
            firstdiffindex = i as usize;
            break;
        }
    }

    firstdiffindex
}

// Insert a `(key,value)` pair into a (sub-)tree represented by `root`.
// It returns the root of the updated (sub-)tree.
pub fn insert_leaf(root: &mut Node, key: Vec<u8>, value: Vec<u8>) -> Result<Node, String> {
    use Node::*;

    if key.len() == 0 {
        return Err("Attempted to insert a 0-byte key".to_string());
    }

    match root {
        Leaf(leafkey, leafvalue) => {
            // Find the common part of the current key with that of the
            // leaf and create an intermediate full node.
            let firstdiffindex = leafkey.factor_length(&NibbleKey::new(key.clone()));

            // Return an error if the leaf is already present.
            if firstdiffindex == key.len() {
                return Err(format!("Key is is already present!",));
            }

            // Create the new root, which is a full node.
            let mut res = vec![EmptySlot; 16];
            // Add the initial leaf, with a key truncated by the common
            // key part.
            res[leafkey[firstdiffindex] as usize] =
                Leaf(leafkey.remove_prefix(firstdiffindex), leafvalue.to_vec());
            // Add the node to be inserted
            res[key[firstdiffindex] as usize] =
                Leaf(NibbleKey::new(key[firstdiffindex + 1..].to_vec()), value);
            // Put the common part into an extension node
            if firstdiffindex == 0 {
                // Special case: no extension necessary
                Ok(FullNode(res))
            } else {
                Ok(Extension(
                    key[..firstdiffindex].to_vec(),
                    Box::new(FullNode(res)),
                ))
            }
        }
        Extension(extkey, box child) => {
            // Find the common part of the current key with that of the
            // extension and create an intermediate full node.
            let firstdiffindex = find_common_length(&key, &extkey);

            assert!(firstdiffindex <= extkey.len());
            assert!(firstdiffindex <= key.len());

            // Special case: key is longer than the extension key:
            // recurse on the child node.
            if firstdiffindex == extkey.len() {
                let childroot =
                    insert_leaf(&mut child.clone(), key[extkey.len()..].to_vec(), value)?;
                return Ok(Extension(extkey.to_vec(), Box::new(childroot)));
            }

            // Special case: key is completely unlike the extension key
            if firstdiffindex == 0 {
                let mut res = vec![EmptySlot; 16];

                // Create the entry for the truncated extension key
                // Was it an extension of 1 ? If so, place the node directly
                // otherwise truncate the extension.
                res[extkey[0] as usize] = if extkey.len() == 1 {
                    child.clone()
                } else {
                    Extension(extkey[1..].to_vec(), Box::new(child.clone()))
                };

                // Create the entry for the node. If there was only a
                // difference of one byte, that byte will be consumed by
                // the fullnode and therefore the key in the leaf will be
                // an empty slice `[]`.
                res[key[0] as usize] = Leaf(NibbleKey::new(key[1..].to_vec()), value);

                return Ok(FullNode(res));
            }

            // Create the new root, which is a full node.
            let mut res = vec![EmptySlot; 16];
            // Add the initial leaf, with a key truncated by the common
            // key part. If the common part corresponds to the extension
            // key length minus one, then there is no need for the creation
            // of an extension node past the full node.
            res[extkey[firstdiffindex] as usize] = if extkey.len() - firstdiffindex > 1 {
                Extension(
                    extkey[firstdiffindex + 1..].to_vec(),
                    Box::new(child.clone()),
                )
            } else {
                child.clone()
            };
            // Add the node to be inserted
            res[key[firstdiffindex] as usize] =
                Leaf(NibbleKey::new(key[firstdiffindex + 1..].to_vec()), value);
            // Put the common part into an extension node
            Ok(Extension(
                extkey[..firstdiffindex].to_vec(),
                Box::new(FullNode(res)),
            ))
        }
        FullNode(ref mut vec) => {
            let idx = key[0] as usize;
            // If the slot isn't yet in use, fill it, and otherwise,
            // recurse into the child node.
            vec[idx] = if vec[idx] == EmptySlot {
                // XXX check that the value is at least 1
                Leaf(NibbleKey::new(key[1..].to_vec()), value)
            } else {
                insert_leaf(&mut vec[idx], key[idx + 1..].to_vec(), value)?
            };
            // Return the root node with an updated entry
            Ok(FullNode(vec.to_vec()))
        }
        _ => panic!("Not supported yet"),
    }
}

// Helper function that generates a multiproof based on one `(key.value)`
// pair.
pub fn make_multiproof(
    root: &Node,
    keyvals: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<Multiproof, String> {
    use Node::*;

    let mut instructions = Vec::new();
    let mut values = Vec::new();
    let mut hashes = Vec::new();

    // If there are no keys specified at this node, then just hash that
    // node.
    if keyvals.len() == 0 {
        return Ok(Multiproof {
            instructions: vec![Instruction::HASHER(0)],
            hashes: vec![root.hash(&mut vec![])],
            keyvals: vec![],
        });
    }

    // Recurse into each node, follow the trace
    match root {
        EmptySlot => return Err("Cannot build a multiproof on an empty slot".to_string()),
        FullNode(ref vec) => {
            // Split the current (key,value) tuples based on the first
            // nibble of their keys. Build a recursion table.
            let mut split = vec![Vec::new(); 16];
            for (k, v) in keyvals.iter() {
                let idx = k[0] as usize;
                split[idx].push((k[1..].to_vec(), v.to_vec()));
            }

            // Now recurse on each selector. If the recursion table is
            // empty, then the subnode needs to be hashed. Otherwise,
            // we must recurse.
            // `branch` is set to true at first, which is meant to add
            // a `BRANCH` instruction the first time that a child is
            // added to the node. All subsequent adds will be performed
            // by an `ADD` instruction.
            let mut branch = true;
            for (selector, subkeys) in split.iter().enumerate() {
                // Does the child have any key? If not, it will be hashed
                // and a `HASHER` instruction will be added to the list.
                if split[selector].len() == 0 {
                    // Empty slots are not to be hashed
                    if vec[selector] != EmptySlot {
                        instructions.push(Instruction::HASHER(0));
                        instructions.push(Instruction::ADD(selector));
                        hashes.push(vec[selector].hash(&mut vec![]));
                    }
                } else {
                    let mut proof = make_multiproof(&vec[selector], subkeys.to_vec())?;
                    instructions.append(&mut proof.instructions);
                    if branch {
                        instructions.push(Instruction::BRANCH(selector));
                        branch = false;
                    } else {
                        instructions.push(Instruction::ADD(selector));
                    }
                    hashes.append(&mut proof.hashes);
                    values.append(&mut proof.keyvals);
                }
            }
        }
        Leaf(leafkey, _) => {
            if keyvals.len() != 1 {
                return Err(format!(
                    "Expecting exactly 1 key in leaf, got {}: {:?}",
                    keyvals.len(),
                    keyvals
                )
                .to_string());
            }

            let key = &keyvals[0].0;
            if *leafkey == NibbleKey::new(key.to_vec()) {
                instructions.push(Instruction::LEAF(key.len()));
                let rlp = rlp::encode(&Leaf(NibbleKey::new(key.clone()), keyvals[0].1.clone()));
                values.push(rlp);
            } else {
                return Err(
                    format!("Trying to apply the wrong key {:?} != {:?}", key, leafkey).to_string(),
                );
            }
        }
        Extension(extkey, box child) => {
            // Make sure that all the keys follow the extension and
            // if so, then recurse.
            let mut truncated = vec![];
            for (k, v) in keyvals.iter() {
                if &k[..extkey.len()] != &extkey[..] {
                    return Err(
                        format!("One of the keys isn't present in the tree: {:?}", k).to_string(),
                    );
                }
                truncated.push((k.to_vec(), v.to_vec()));
            }
            let mut proof = make_multiproof(child, truncated)?;
            hashes.append(&mut proof.hashes);
            instructions.append(&mut proof.instructions);
            values.append(&mut proof.keyvals);
        }
        Hash(_, _) => return Err("Should not have encountered a Hash in this context".to_string()),
    }

    Ok(Multiproof {
        instructions: instructions,
        hashes: hashes,
        keyvals: values,
    })
}

#[cfg(test)]
mod tests {
    extern crate hex;
    //extern crate rand;

    use super::Instruction::*;
    use super::Node::*;
    use super::*;
    //use rand::prelude::*;

    #[test]
    fn validate_tree() {
        let mut root = FullNode(vec![EmptySlot; 16]);
        insert_leaf(&mut root, vec![2u8; 32], vec![0u8; 32]).unwrap();
        insert_leaf(&mut root, vec![1u8; 32], vec![1u8; 32]).unwrap();
        insert_leaf(&mut root, vec![8u8; 32], vec![150u8; 32]).unwrap();

        let changes = vec![
            (vec![2u8; 32], vec![4u8; 32]),
            (vec![1u8; 32], vec![8u8; 32]),
        ];

        let proof = make_multiproof(&root, changes.clone()).unwrap();

        let mut stack = Vec::new();
        let proof = Multiproof {
            hashes: proof.hashes,
            keyvals: proof.keyvals,
            instructions: proof.instructions,
        };
        let new_root = rebuild(&mut stack, &proof);

        assert_eq!(
            new_root,
            FullNode(vec![
                EmptySlot,
                Leaf(
                    NibbleKey::new(vec![
                        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                        1, 1, 1, 1, 1, 1
                    ]),
                    vec![
                        8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
                        8, 8, 8, 8, 8, 8, 8
                    ]
                ),
                Leaf(
                    NibbleKey::new(vec![
                        2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
                        2, 2, 2, 2, 2, 2
                    ]),
                    vec![
                        4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
                        4, 4, 4, 4, 4, 4, 4
                    ]
                ),
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                Hash(
                    vec![
                        148, 246, 44, 213, 233, 204, 109, 50, 27, 235, 77, 70, 96, 129, 48, 141,
                        228, 36, 161, 5, 222, 36, 15, 220, 35, 103, 15, 4, 65, 254, 67, 132
                    ],
                    0
                ),
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot
            ])
        );
    }

    #[test]
    fn make_multiproof_two_values() {
        let mut root = FullNode(vec![EmptySlot; 16]);
        insert_leaf(&mut root, vec![2u8; 32], vec![0u8; 32]).unwrap();
        insert_leaf(&mut root, vec![1u8; 32], vec![1u8; 32]).unwrap();
        insert_leaf(&mut root, vec![8u8; 32], vec![150u8; 32]).unwrap();

        let proof = make_multiproof(
            &root,
            vec![
                (vec![2u8; 32], vec![4u8; 32]),
                (vec![1u8; 32], vec![8u8; 32]),
            ],
        )
        .unwrap();
        let i = proof.instructions;
        let v = proof.keyvals;
        let h = proof.hashes;
        assert_eq!(i.len(), 6); // [LEAF, BRANCH, LEAF, ADD, HASHER, ADD]
        match i[0] {
            // Key length is 31
            LEAF(n) => assert_eq!(n, 31),
            _ => panic!(format!("Invalid instruction {:?}", i[0])),
        }
        match i[1] {
            BRANCH(n) => assert_eq!(n, 1),
            _ => panic!(format!("Invalid instruction {:?}", i[1])),
        }
        match i[2] {
            // Key length is 31
            LEAF(n) => assert_eq!(n, 31),
            _ => panic!(format!("Invalid instruction {:?}", i[2])),
        }
        match i[3] {
            ADD(n) => assert_eq!(n, 2),
            _ => panic!(format!("Invalid instruction {:?}", i[3])),
        }
        match i[5] {
            ADD(n) => assert_eq!(n, 8),
            _ => panic!(format!("Invalid instruction {:?}", i[5])),
        }
        assert_eq!(h.len(), 1); // Only one hash
        assert_eq!(v.len(), 2);
        assert_eq!(
            v[0],
            rlp::encode(&Leaf(NibbleKey::new(vec![1u8; 31]), vec![8u8; 32]))
        );
        assert_eq!(
            v[1],
            rlp::encode(&Leaf(NibbleKey::new(vec![2u8; 31]), vec![4u8; 32]))
        );
    }

    #[test]
    fn make_multiproof_single_value() {
        let mut root = FullNode(vec![EmptySlot; 16]);
        insert_leaf(&mut root, vec![2u8; 32], vec![0u8; 32]).unwrap();
        insert_leaf(&mut root, vec![1u8; 32], vec![1u8; 32]).unwrap();

        let proof = make_multiproof(&root, vec![(vec![1u8; 32], vec![1u8; 32])]).unwrap();
        let i = proof.instructions;
        let v = proof.keyvals;
        let h = proof.hashes;
        assert_eq!(i.len(), 4); // [LEAF, BRANCH, HASHER, ADD]
        match i[0] {
            // Key length is 31
            LEAF(n) => assert_eq!(n, 31),
            _ => panic!(format!("Invalid instruction {:?}", i[0])),
        }
        match i[1] {
            BRANCH(n) => assert_eq!(n, 1),
            _ => panic!(format!("Invalid instruction {:?}", i[1])),
        }
        match i[2] {
            HASHER(n) => assert_eq!(n, 0),
            _ => panic!(format!("Invalid instruction {:?}", i[2])),
        }
        match i[3] {
            ADD(n) => assert_eq!(n, 2),
            _ => panic!(format!("Invalid instruction {:?}", i[3])),
        }
        assert_eq!(h.len(), 1); // Only one hash
        assert_eq!(v.len(), 1); // Only one value
        assert_eq!(
            v[0],
            rlp::encode(&Leaf(NibbleKey::new(vec![1u8; 31]), vec![1u8; 32]))
        );
    }

    #[test]
    fn make_multiproof_no_values() {
        let mut root = FullNode(vec![EmptySlot; 16]);
        insert_leaf(&mut root, vec![2u8; 32], vec![0u8; 32]).unwrap();
        insert_leaf(&mut root, vec![1u8; 32], vec![1u8; 32]).unwrap();

        let proof = make_multiproof(&root, vec![]).unwrap();
        let i = proof.instructions;
        let v = proof.keyvals;
        let h = proof.hashes;
        assert_eq!(i.len(), 1);
        assert_eq!(h.len(), 1);
        assert_eq!(v.len(), 0);
    }

    #[test]
    fn make_multiproof_empty_tree() {
        let root = FullNode(vec![EmptySlot; 16]);

        let out = make_multiproof(&root, vec![(vec![1u8; 32], vec![1u8; 32])]);
        assert!(out.is_err());
    }

    #[test]
    fn insert_leaf_zero_length_key_after_fullnode() {
        let mut root = Extension(
            vec![0u8; 31],
            Box::new(FullNode(vec![
                EmptySlot,
                Leaf(NibbleKey::new(vec![]), vec![0u8; 32]),
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
            ])),
        );
        let out = insert_leaf(&mut root, vec![0u8; 32], vec![1u8; 32]).unwrap();
        assert_eq!(
            out,
            Extension(
                vec![0u8; 31],
                Box::new(FullNode(vec![
                    Leaf(NibbleKey::new(vec![]), vec![1u8; 32]),
                    Leaf(NibbleKey::new(vec![]), vec![0u8; 32]),
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot
                ]))
            )
        );
    }

    #[test]
    fn insert_leaf_into_extension_root_all_bytes_in_key_common() {
        let mut root = Extension(
            vec![0xd, 0xe, 0xa, 0xd],
            Box::new(Leaf(NibbleKey::new(vec![0u8; 28]), vec![1u8; 32])),
        );
        let mut key = vec![1u8; 32];
        key[0] = 0xd;
        key[1] = 0xe;
        key[2] = 0xa;
        key[3] = 0xd;
        let out = insert_leaf(&mut root, key, vec![1u8; 32]).unwrap();
        assert_eq!(
            out,
            Extension(
                vec![0xd, 0xe, 0xa, 0xd],
                Box::new(FullNode(vec![
                    Leaf(NibbleKey::new(vec![0u8; 27]), vec![1u8; 32]),
                    Leaf(NibbleKey::new(vec![1u8; 27]), vec![1u8; 32]),
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot
                ]))
            )
        );
    }

    #[test]
    fn insert_leaf_into_extension_root_no_common_bytes_in_key() {
        let mut root = Extension(
            vec![0xd, 0xe, 0xa, 0xd],
            Box::new(Leaf(NibbleKey::new(vec![0u8; 24]), vec![1u8; 32])),
        );
        let out = insert_leaf(&mut root, vec![2u8; 32], vec![1u8; 32]).unwrap();
        assert_eq!(
            out,
            FullNode(vec![
                EmptySlot,
                EmptySlot,
                Leaf(NibbleKey::new(vec![2u8; 31]), vec![1u8; 32]),
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                Extension(
                    vec![14, 10, 13],
                    Box::new(Leaf(NibbleKey::new(vec![0u8; 24]), vec![1u8; 32]))
                ),
                EmptySlot,
                EmptySlot
            ])
        );
    }

    #[test]
    fn insert_leaf_into_extension_root_half_bytes_in_key_common() {
        let mut root = Extension(
            vec![0xd, 0xe, 0xa, 0xd],
            Box::new(Leaf(NibbleKey::new(vec![0u8; 28]), vec![1u8; 32])),
        );
        let mut key = vec![0u8; 32];
        key[0] = 0xd;
        key[1] = 0xe;
        let out = insert_leaf(&mut root, key, vec![1u8; 32]).unwrap();
        assert_eq!(
            out,
            Extension(
                vec![0xd, 0xe],
                Box::new(FullNode(vec![
                    Leaf(NibbleKey::new(vec![0u8; 29]), vec![1u8; 32]),
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    Extension(
                        vec![0xd],
                        Box::new(Leaf(NibbleKey::new(vec![0u8; 28]), vec![1u8; 32]))
                    ),
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot
                ]))
            )
        );
    }

    #[test]
    fn insert_leaf_into_extension_root_almost_all_bytes_in_key_common() {
        let mut root = Extension(
            vec![0xd, 0xe, 0xa, 0xd],
            Box::new(Leaf(NibbleKey::new(vec![0u8; 28]), vec![1u8; 32])),
        );
        let mut key = vec![0u8; 32];
        key[0] = 0xd;
        key[1] = 0xe;
        key[2] = 0xa;
        let out = insert_leaf(&mut root, key, vec![1u8; 32]).unwrap();
        assert_eq!(
            out,
            Extension(
                vec![0xd, 0xe, 0xa],
                Box::new(FullNode(vec![
                    Leaf(NibbleKey::new(vec![0u8; 28]), vec![1u8; 32]),
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    Leaf(NibbleKey::new(vec![0u8; 28]), vec![1u8; 32]),
                    EmptySlot,
                    EmptySlot
                ]))
            )
        );
    }

    #[test]
    fn insert_leaf_into_leaf_root_common_bytes_in_key() {
        let mut key = vec![0u8; 32];
        for (i, v) in key.iter_mut().enumerate() {
            if i >= 16 {
                break;
            }
            *v = 2u8;
        }
        let mut root = Leaf(NibbleKey::new(key), vec![1u8; 32]);
        let out = insert_leaf(&mut root, vec![2u8; 32], vec![1u8; 32]).unwrap();
        assert_eq!(
            out,
            Extension(
                vec![2u8; 16],
                Box::new(FullNode(vec![
                    Leaf(NibbleKey::new(vec![0u8; 15]), vec![1u8; 32]),
                    EmptySlot,
                    Leaf(NibbleKey::new(vec![2u8; 15]), vec![1u8; 32]),
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot
                ]))
            )
        );
    }

    #[test]
    fn insert_leaf_into_leaf_root_no_common_bytes_in_key() {
        let mut root = Leaf(NibbleKey::new(vec![1u8; 32]), vec![1u8; 32]);
        let out = insert_leaf(&mut root, vec![2u8; 32], vec![1u8; 32]).unwrap();
        assert_eq!(
            out,
            FullNode(vec![
                EmptySlot,
                Leaf(NibbleKey::new(vec![1u8; 31]), vec![1u8; 32]),
                Leaf(NibbleKey::new(vec![2u8; 31]), vec![1u8; 32]),
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot
            ])
        );
    }

    #[test]
    fn insert_leaf_into_empty_root() {
        let children = vec![EmptySlot; 16];
        let mut root = FullNode(children);
        let out = insert_leaf(&mut root, vec![0u8; 32], vec![1u8; 32]);
        assert_eq!(
            out.unwrap(),
            FullNode(vec![
                Leaf(NibbleKey::new(vec![0u8; 31]), vec![1u8; 32]),
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot
            ])
        );
    }

    #[test]
    fn insert_leaf_into_two_level_fullnodes() {
        let mut root = FullNode(vec![
            FullNode(vec![EmptySlot; 16]),
            EmptySlot,
            EmptySlot,
            EmptySlot,
            EmptySlot,
            EmptySlot,
            EmptySlot,
            EmptySlot,
            EmptySlot,
            EmptySlot,
            EmptySlot,
            EmptySlot,
            EmptySlot,
            EmptySlot,
            EmptySlot,
            EmptySlot,
        ]);
        let out = insert_leaf(&mut root, vec![0u8; 32], vec![1u8; 32]);
        assert_eq!(
            out.unwrap(),
            FullNode(vec![
                FullNode(vec![
                    Leaf(NibbleKey::new(vec![0u8; 30]), vec![1u8; 32]),
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot
                ]),
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot
            ])
        );
    }

    #[test]
    fn tree_with_just_one_leaf() {
        let mut stack = Vec::new();
        let proof = Multiproof {
            hashes: vec![],
            keyvals: vec![rlp::encode_list::<Vec<u8>, Vec<u8>>(&vec![
                vec![1, 2, 3],
                vec![4, 5, 6],
            ])],
            instructions: vec![LEAF(0)],
        };
        let out = rebuild(&mut stack, &proof);
        assert_eq!(out, Leaf(NibbleKey::new(vec![]), vec![4, 5, 6]))
    }

    #[test]
    fn tree_with_one_branch() {
        let mut stack = Vec::new();
        let proof = Multiproof {
            hashes: vec![],
            keyvals: vec![rlp::encode_list::<Vec<u8>, Vec<u8>>(&vec![
                vec![1, 2, 3],
                vec![4, 5, 6],
            ])],
            instructions: vec![LEAF(0), BRANCH(0)],
        };
        let out = rebuild(&mut stack, &proof);
        assert_eq!(
            out,
            FullNode(vec![
                Leaf(NibbleKey::new(vec![]), vec![4, 5, 6]),
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot
            ])
        )
    }

    #[test]
    fn tree_with_added_branch() {
        let mut stack = Vec::new();
        let proof = Multiproof {
            hashes: vec![],
            keyvals: vec![
                rlp::encode_list::<Vec<u8>, Vec<u8>>(&vec![vec![1, 2, 3], vec![4, 5, 6]]),
                rlp::encode_list::<Vec<u8>, Vec<u8>>(&vec![vec![7, 8, 9], vec![10, 11, 12]]),
            ],
            instructions: vec![LEAF(0), BRANCH(0), LEAF(1), ADD(2)],
        };
        let out = rebuild(&mut stack, &proof);
        assert_eq!(
            out,
            FullNode(vec![
                Leaf(NibbleKey::new(vec![]), vec![4, 5, 6]),
                EmptySlot,
                Leaf(NibbleKey::new(vec![9]), vec![10, 11, 12]),
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot
            ])
        )
    }

    #[test]
    fn tree_with_extension() {
        let mut stack = Vec::new();
        let proof = Multiproof {
            hashes: vec![],
            instructions: vec![
                LEAF(0),
                BRANCH(0),
                LEAF(1),
                ADD(2),
                EXTENSION(vec![13, 14, 15]),
            ],
            keyvals: vec![
                rlp::encode_list::<Vec<u8>, Vec<u8>>(&vec![vec![1, 2, 3], vec![4, 5, 6]]),
                rlp::encode_list::<Vec<u8>, Vec<u8>>(&vec![vec![7, 8, 9], vec![10, 11, 12]]),
            ],
        };
        let out = rebuild(&mut stack, &proof);
        assert_eq!(
            out,
            Extension(
                vec![13, 14, 15],
                Box::new(FullNode(vec![
                    Leaf(NibbleKey::new(vec![]), vec![4, 5, 6]),
                    EmptySlot,
                    Leaf(NibbleKey::new(vec![9]), vec![10, 11, 12]),
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot,
                    EmptySlot
                ]))
            )
        )
    }

    #[test]
    fn single_value_hash() {
        let mut hashers = Vec::new();
        assert_eq!(
            Leaf(NibbleKey::new(vec![1, 2, 3]), vec![4, 5, 6]).hash(&mut hashers),
            vec![194, 131, 1, 2, 3, 131, 4, 5, 6]
        );
    }

    #[test]
    fn big_value_single_key_hash() {
        let mut hashers = Vec::new();
        assert_eq!(
            Leaf(NibbleKey::new(vec![0u8; 32]), vec![4, 5, 6]).hash(&mut hashers),
            vec![
                131, 176, 193, 69, 224, 210, 235, 150, 232, 34, 23, 122, 33, 191, 215, 245, 166,
                14, 84, 130, 80, 200, 156, 109, 242, 82, 179, 107, 99, 126, 138, 48
            ]
        );
    }

    #[test]
    fn big_value_single_big_key_hash() {
        let mut hashers = Vec::new();
        assert_eq!(
            Leaf(NibbleKey::new(vec![0u8; 32]), vec![1u8; 32]).hash(&mut hashers),
            vec![
                46, 13, 98, 250, 109, 96, 126, 167, 238, 29, 122, 212, 177, 83, 107, 74, 122, 19,
                242, 93, 2, 118, 56, 156, 108, 100, 76, 183, 135, 237, 157, 192
            ]
        );
    }

    #[test]
    fn empty_value_hash() {
        let mut hashers = Vec::new();
        let node = EmptySlot;
        assert_eq!(node.hash(&mut hashers), vec![]);
    }

    #[test]
    fn full_node_hash() {
        let mut hashers = Vec::new();
        assert_eq!(
            FullNode(vec![
                Leaf(NibbleKey::new(vec![]), vec![4, 5, 6]),
                EmptySlot,
                Leaf(NibbleKey::new(vec![9]), vec![10, 11, 12]),
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot,
                EmptySlot
            ])
            .hash(&mut hashers),
            vec![
                220, 134, 193, 128, 131, 4, 5, 6, 128, 134, 193, 9, 131, 10, 11, 12, 128, 128, 128,
                128, 128, 128, 128, 128, 128, 128, 128, 128, 128
            ]
        );
    }
}
