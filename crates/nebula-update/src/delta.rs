//! Binary delta patches.
//!
//! The format is deliberately simple: a sequence of copy-from-old and
//! insert-literal instructions, compressed with zstd. It is not as compact as
//! bsdiff, but it is a few hundred lines rather than a C dependency, it is
//! obviously correct by inspection, and it achieves the thing that actually
//! matters — a release that changes one function ships as kilobytes rather than
//! as the whole binary.
//!
//! A patch is never trusted on its own. [`apply_patch`] verifies the result
//! against the hash of the new release, so a malicious patch cannot produce
//! anything other than the bytes the manifest committed to.

use serde::{Deserialize, Serialize};

use crate::{Result, UpdateError, hash};

/// One instruction in a patch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Instruction {
    /// Copy `length` bytes from the old file starting at `offset`.
    Copy {
        /// Where in the old file.
        offset: u64,
        /// How many bytes.
        length: u64,
    },
    /// Insert literal bytes not present in the old file.
    Insert {
        /// The bytes.
        data: Vec<u8>,
    },
}

/// A delta between two versions of a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    /// Hash of the file this patch applies to.
    pub from_hash: String,
    /// Hash of the file it produces.
    pub to_hash: String,
    /// Size of the resulting file.
    pub to_size: u64,
    /// The instructions.
    pub instructions: Vec<Instruction>,
}

/// The block size used when looking for matches.
///
/// Small blocks find more matches and produce a bigger instruction list; 4 KiB
/// is a reasonable balance for executables, where changes cluster.
const BLOCK: usize = 4096;

impl Patch {
    /// Serialise and compress.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let json = serde_json::to_vec(self)
            .map_err(|e| UpdateError::Patch(format!("serialising the patch: {e}")))?;
        zstd::encode_all(json.as_slice(), 3)
            .map_err(|e| UpdateError::Patch(format!("compressing the patch: {e}")))
    }

    /// Decompress and parse.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let json = zstd::decode_all(bytes)
            .map_err(|e| UpdateError::Patch(format!("decompressing the patch: {e}")))?;
        serde_json::from_slice(&json)
            .map_err(|e| UpdateError::Patch(format!("parsing the patch: {e}")))
    }

    /// How many bytes this patch carries as literals.
    pub fn literal_bytes(&self) -> usize {
        self.instructions
            .iter()
            .map(|instruction| match instruction {
                Instruction::Insert { data } => data.len(),
                Instruction::Copy { .. } => 0,
            })
            .sum()
    }
}

/// Build a patch that turns `old` into `new`.
pub fn make_patch(old: &[u8], new: &[u8]) -> Patch {
    // Index the old file by block content, so a moved or unchanged block can be
    // found in constant time.
    let mut blocks: std::collections::HashMap<&[u8], u64> = std::collections::HashMap::new();
    let mut offset = 0usize;
    while offset + BLOCK <= old.len() {
        // First occurrence wins, which keeps offsets low and the patch stable.
        blocks.entry(&old[offset..offset + BLOCK]).or_insert(offset as u64);
        offset += BLOCK;
    }

    let mut instructions: Vec<Instruction> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut position = 0usize;

    while position < new.len() {
        let remaining = new.len() - position;
        if remaining < BLOCK {
            pending.extend_from_slice(&new[position..]);
            break;
        }

        match blocks.get(&new[position..position + BLOCK]) {
            Some(&found) => {
                // Extend the match as far as it goes, so a long unchanged run
                // becomes one instruction rather than many.
                let mut length = BLOCK;
                while position + length < new.len()
                    && (found as usize + length) < old.len()
                    && new[position + length] == old[found as usize + length]
                {
                    length += 1;
                }

                if !pending.is_empty() {
                    instructions.push(Instruction::Insert { data: std::mem::take(&mut pending) });
                }
                instructions.push(Instruction::Copy { offset: found, length: length as u64 });
                position += length;
            }
            None => {
                // No match at this position: accumulate one byte and try again.
                // Byte-at-a-time realignment is what lets an insertion in the
                // middle of a file resynchronise with the following blocks.
                pending.push(new[position]);
                position += 1;
            }
        }
    }

    if !pending.is_empty() {
        instructions.push(Instruction::Insert { data: pending });
    }

    Patch {
        from_hash: hash(old),
        to_hash: hash(new),
        to_size: new.len() as u64,
        instructions,
    }
}

/// Apply `patch` to `old`, verifying the result.
///
/// Three checks, all of which must pass:
///
/// 1. `old` is the file the patch was built against;
/// 2. every instruction stays inside the old file's bounds;
/// 3. the result hashes to what the patch promised.
///
/// The third is the one that matters. It means a patch cannot produce arbitrary
/// output even if the first two were somehow satisfied.
pub fn apply_patch(old: &[u8], patch: &Patch) -> Result<Vec<u8>> {
    let old_hash = hash(old);
    if old_hash != patch.from_hash {
        return Err(UpdateError::Patch(format!(
            "this patch applies to {} but the installed file is {old_hash}",
            patch.from_hash
        )));
    }

    let mut out = Vec::with_capacity(patch.to_size as usize);
    for instruction in &patch.instructions {
        match instruction {
            Instruction::Copy { offset, length } => {
                let start = *offset as usize;
                let end = start
                    .checked_add(*length as usize)
                    .ok_or_else(|| UpdateError::Patch("copy length overflows".to_string()))?;
                if end > old.len() {
                    return Err(UpdateError::Patch(format!(
                        "copy instruction reads past the end of the old file ({end} > {})",
                        old.len()
                    )));
                }
                out.extend_from_slice(&old[start..end]);
            }
            Instruction::Insert { data } => out.extend_from_slice(data),
        }

        // A patch that claims a small result but emits gigabytes would exhaust
        // memory before the final hash check could reject it.
        if out.len() as u64 > patch.to_size {
            return Err(UpdateError::Patch(format!(
                "patch produced more than the {} bytes it declared",
                patch.to_size
            )));
        }
    }

    let produced = hash(&out);
    if produced != patch.to_hash {
        return Err(UpdateError::HashMismatch { expected: patch.to_hash.clone(), actual: produced });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic binary: repetitive like a real executable, so block matching
    /// behaves the way it would in production.
    fn binary(seed: u64, size: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(size);
        let mut state = seed | 1;
        while out.len() < size {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(size);
        out
    }

    #[test]
    fn a_patch_reconstructs_the_new_file_exactly() {
        let old = binary(1, 256 * 1024);
        let mut new = old.clone();
        // Change a function-sized region in the middle.
        new[100_000..101_000].copy_from_slice(&binary(2, 1000));

        let patch = make_patch(&old, &new);
        assert_eq!(apply_patch(&old, &patch).unwrap(), new);
    }

    #[test]
    fn a_small_change_produces_a_small_patch() {
        // The entire point of shipping deltas.
        let old = binary(1, 1024 * 1024);
        let mut new = old.clone();
        new[500_000..500_100].copy_from_slice(&binary(9, 100));

        let patch = make_patch(&old, &new);
        let encoded = patch.to_bytes().unwrap();

        assert!(
            encoded.len() < old.len() / 10,
            "a 100-byte change produced a {} byte patch against a {} byte file",
            encoded.len(),
            old.len()
        );
    }

    #[test]
    fn an_insertion_resynchronises_with_the_rest_of_the_file() {
        // Naive block matching fails here: everything after the insertion is
        // shifted and no block boundary lines up.
        let old = binary(1, 512 * 1024);
        let mut new = Vec::new();
        new.extend_from_slice(&old[..200_000]);
        new.extend_from_slice(b"newly inserted bytes that shift everything after them");
        new.extend_from_slice(&old[200_000..]);

        let patch = make_patch(&old, &new);
        assert_eq!(apply_patch(&old, &patch).unwrap(), new);

        let encoded = patch.to_bytes().unwrap();
        assert!(
            encoded.len() < old.len() / 4,
            "an insertion should still delta well, got {} bytes",
            encoded.len()
        );
    }

    #[test]
    fn a_deletion_patches_correctly() {
        let old = binary(1, 512 * 1024);
        let mut new = Vec::new();
        new.extend_from_slice(&old[..100_000]);
        new.extend_from_slice(&old[150_000..]);

        let patch = make_patch(&old, &new);
        assert_eq!(apply_patch(&old, &patch).unwrap(), new);
    }

    #[test]
    fn a_completely_different_file_still_patches_correctly() {
        let old = binary(1, 64 * 1024);
        let new = binary(999, 64 * 1024);

        let patch = make_patch(&old, &new);
        assert_eq!(apply_patch(&old, &patch).unwrap(), new);
        // With nothing in common the patch is mostly literals, which is correct.
        assert!(patch.literal_bytes() > new.len() / 2);
    }

    #[test]
    fn applying_to_the_wrong_base_is_refused() {
        // A patch built against 1.0 must not be applied to a modified 1.0.
        let old = binary(1, 64 * 1024);
        let new = binary(2, 64 * 1024);
        let patch = make_patch(&old, &new);

        let mut tampered = old.clone();
        tampered[0] ^= 0xFF;

        let err = apply_patch(&tampered, &patch).unwrap_err();
        assert!(
            err.to_string().contains("applies to"),
            "the mismatch should be explained: {err}"
        );
    }

    #[test]
    fn a_patch_whose_result_does_not_match_its_hash_is_refused() {
        // The check that stops a malicious patch producing arbitrary output.
        let old = binary(1, 64 * 1024);
        let new = binary(2, 64 * 1024);
        let mut patch = make_patch(&old, &new);

        // Smuggle extra bytes in, and adjust the declared size so the length
        // guard does not catch it first.
        patch.instructions.push(Instruction::Insert { data: b"backdoor".to_vec() });
        patch.to_size += 8;

        let err = apply_patch(&old, &patch).unwrap_err();
        assert!(
            matches!(err, UpdateError::HashMismatch { .. }),
            "expected a hash mismatch, got {err:?}"
        );
    }

    #[test]
    fn a_copy_past_the_end_of_the_old_file_is_refused() {
        let old = binary(1, 4096);
        let new = binary(2, 4096);
        let mut patch = make_patch(&old, &new);

        patch.instructions = vec![Instruction::Copy { offset: 0, length: 1_000_000 }];
        let err = apply_patch(&old, &patch).unwrap_err();
        assert!(err.to_string().contains("past the end"), "{err}");
    }

    #[test]
    fn an_overflowing_copy_length_is_refused() {
        let old = binary(1, 4096);
        let patch = Patch {
            from_hash: hash(&old),
            to_hash: hash(b""),
            to_size: 0,
            instructions: vec![Instruction::Copy { offset: u64::MAX, length: u64::MAX }],
        };
        assert!(apply_patch(&old, &patch).is_err());
    }

    #[test]
    fn a_patch_claiming_a_small_result_cannot_emit_gigabytes() {
        // Without the running length check this would allocate until the process
        // died, before the final hash could reject it.
        let old = binary(1, 4096);
        let patch = Patch {
            from_hash: hash(&old),
            to_hash: hash(b"small"),
            to_size: 5,
            instructions: (0..1000)
                .map(|_| Instruction::Insert { data: vec![0u8; 100_000] })
                .collect(),
        };

        let err = apply_patch(&old, &patch).unwrap_err();
        assert!(err.to_string().contains("more than the 5 bytes"), "{err}");
    }

    #[test]
    fn patches_round_trip_through_their_wire_format() {
        let old = binary(1, 128 * 1024);
        let new = binary(2, 128 * 1024);
        let patch = make_patch(&old, &new);

        let restored = Patch::from_bytes(&patch.to_bytes().unwrap()).unwrap();
        assert_eq!(restored, patch);
        assert_eq!(apply_patch(&old, &restored).unwrap(), new);
    }

    #[test]
    fn corrupt_patch_bytes_are_reported_rather_than_panicking() {
        assert!(Patch::from_bytes(b"not zstd").is_err());
        assert!(Patch::from_bytes(&[]).is_err());
    }

    #[test]
    fn empty_files_are_handled() {
        let patch = make_patch(&[], &[]);
        assert_eq!(apply_patch(&[], &patch).unwrap(), Vec::<u8>::new());

        let new = binary(1, 1000);
        let patch = make_patch(&[], &new);
        assert_eq!(apply_patch(&[], &patch).unwrap(), new);
    }

    #[test]
    fn an_identical_file_produces_a_patch_of_pure_copies() {
        let file = binary(1, 256 * 1024);
        let patch = make_patch(&file, &file);

        assert_eq!(patch.literal_bytes(), 0, "nothing changed, so nothing should be literal");
        assert_eq!(apply_patch(&file, &patch).unwrap(), file);
    }
}
