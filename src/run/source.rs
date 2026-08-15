//! Rust source snapshots, revision identity, and patchable literal analysis.

use super::{
    AhoCorasick, ArtifactFileIdentity, BTreeMap, Digest, LiteralIndexEntry, MmapOptions, OsStrExt,
    Path, PathBuf, Range, Read, Sha256, Write, append_context_value, artifact_file_identity,
    artifact_file_identity_from_metadata, changed_format_segment, fs,
};

pub(super) const LITERAL_INDEX_MAGIC: &[u8; 8] = b"CNDX0001";

pub(super) fn build_literal_index(
    directory: &Path,
    sources: &[PathBuf],
    artifact: &Path,
) -> Result<Vec<LiteralIndexEntry>, String> {
    let mut occurrences = BTreeMap::<Vec<u8>, usize>::new();
    for relative in sources {
        let contents = fs::read(directory.join(relative))
            .map_err(|error| format!("could not index {}: {error}", relative.display()))?;
        for candidate in source_literal_candidates(&contents) {
            *occurrences.entry(candidate).or_default() += 1;
        }
    }
    let patterns: Vec<Vec<u8>> = occurrences
        .into_iter()
        .filter_map(|(bytes, count)| (count == 1).then_some(bytes))
        .collect();
    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let matcher = AhoCorasick::new(&patterns)
        .map_err(|error| format!("could not build the literal index: {error}"))?;
    let file = fs::File::open(artifact)
        .map_err(|error| format!("could not index artifact {}: {error}", artifact.display()))?;
    // SAFETY: Cinder owns no writable handle to the executable during this scan.
    // Its metadata is captured before the mapped bytes become visible in state,
    // and later fast-path use rejects artifacts whose metadata has changed.
    let bytes = unsafe { MmapOptions::new().map(&file) }
        .map_err(|error| format!("could not map artifact {}: {error}", artifact.display()))?;
    let mut offsets = vec![None; patterns.len()];
    let mut repeated = vec![false; patterns.len()];
    for found in matcher.find_overlapping_iter(&bytes) {
        let pattern = found.pattern().as_usize();
        if offsets[pattern].is_some() {
            repeated[pattern] = true;
        } else {
            offsets[pattern] = Some(found.start() as u64);
        }
    }
    Ok(patterns
        .into_iter()
        .enumerate()
        .filter_map(|(index, bytes)| {
            if repeated[index] {
                None
            } else {
                offsets[index].map(|offset| LiteralIndexEntry { bytes, offset })
            }
        })
        .collect())
}

pub(super) fn source_literal_candidates(source: &[u8]) -> Vec<Vec<u8>> {
    source_string_literals(source)
        .into_iter()
        .filter_map(|literal| patchable_literal_data(literal.bytes).map(<[u8]>::to_vec))
        .collect()
}

pub(super) struct SourceString<'a> {
    pub(super) bytes: &'a [u8],
    pub(super) range: Range<usize>,
}

/// Finds unescaped UTF-8 string tokens while ignoring comments, character
/// literals, byte/C strings, and raw strings. Cinder deliberately indexes only
/// literals whose source bytes are identical to their compiled bytes.
pub(super) fn source_string_literals(source: &[u8]) -> Vec<SourceString<'_>> {
    let mut literals = Vec::new();
    let mut cursor = 0;
    while cursor < source.len() {
        if source[cursor..].starts_with(b"//") {
            cursor = source[cursor..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(source.len(), |offset| cursor + offset + 1);
            continue;
        }
        if source[cursor..].starts_with(b"/*") {
            cursor = block_comment_end(source, cursor);
            continue;
        }
        if let Some(end) = raw_string_end(source, cursor) {
            cursor = end;
            continue;
        }
        if matches!(source[cursor], b'b' | b'c') && source.get(cursor + 1) == Some(&b'"') {
            cursor = cooked_string_end(source, cursor + 1).map_or(source.len(), |(end, _)| end + 1);
            continue;
        }
        if source[cursor] == b'\'' {
            let Some(end) = char_literal_end(source, cursor) else {
                cursor += 1;
                continue;
            };
            cursor = end;
            continue;
        }
        if source[cursor] != b'"' {
            cursor += 1;
            continue;
        }

        let quote = cursor;
        let Some((end, has_escape)) = cooked_string_end(source, quote) else {
            break;
        };
        let range = quote + 1..end;
        let bytes = &source[range.clone()];
        if !has_escape && !bytes.contains(&b'\n') {
            literals.push(SourceString { bytes, range });
        }
        cursor = end + 1;
    }
    literals
}

pub(super) fn cooked_string_end(source: &[u8], quote: usize) -> Option<(usize, bool)> {
    let mut cursor = quote + 1;
    let mut has_escape = false;
    while cursor < source.len() {
        match source[cursor] {
            b'"' => return Some((cursor, has_escape)),
            b'\\' => {
                has_escape = true;
                cursor += 2;
            }
            _ => cursor += 1,
        }
    }
    None
}

pub(super) fn block_comment_end(source: &[u8], start: usize) -> usize {
    let mut depth = 1usize;
    let mut cursor = start + 2;
    while cursor < source.len() && depth > 0 {
        if source[cursor..].starts_with(b"/*") {
            depth += 1;
            cursor += 2;
        } else if source[cursor..].starts_with(b"*/") {
            depth -= 1;
            cursor += 2;
        } else {
            cursor += 1;
        }
    }
    cursor
}

pub(super) fn raw_string_end(source: &[u8], start: usize) -> Option<usize> {
    let mut cursor = start;
    if matches!(source.get(cursor), Some(b'b' | b'c')) {
        cursor += 1;
    }
    if source.get(cursor) != Some(&b'r') {
        return None;
    }
    cursor += 1;
    let hashes_start = cursor;
    while source.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    if source.get(cursor) != Some(&b'"') {
        return None;
    }
    let hashes = cursor - hashes_start;
    cursor += 1;
    while cursor < source.len() {
        let Some(relative_quote) = source[cursor..].iter().position(|byte| *byte == b'"') else {
            return Some(source.len());
        };
        let quote = cursor + relative_quote;
        let end = quote + 1 + hashes;
        if end <= source.len() && source[quote + 1..end].iter().all(|byte| *byte == b'#') {
            return Some(end);
        }
        cursor = quote + 1;
    }
    Some(source.len())
}

pub(super) fn char_literal_end(source: &[u8], quote: usize) -> Option<usize> {
    let content = quote + 1;
    let first = *source.get(content)?;
    let closing = if first == b'\\' {
        match source.get(content + 1)? {
            b'x' => content + 4,
            b'u' if source.get(content + 2) == Some(&b'{') => {
                content
                    + 3
                    + source[content + 3..]
                        .iter()
                        .position(|byte| *byte == b'}')?
                    + 1
            }
            _ => content + 2,
        }
    } else {
        content + utf8_char_width(first)?
    };
    (source.get(closing) == Some(&b'\'')).then_some(closing + 1)
}

pub(super) const fn utf8_char_width(first: u8) -> Option<usize> {
    match first {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

pub(super) fn patchable_literal_data(literal: &[u8]) -> Option<&[u8]> {
    if literal.len() < 8 || std::str::from_utf8(literal).is_err() {
        return None;
    }
    if literal.contains(&b'{') || literal.contains(&b'}') {
        format_literal_suffix(literal)
    } else {
        Some(literal)
    }
}

pub(super) fn format_literal_suffix(literal: &[u8]) -> Option<&[u8]> {
    let open = literal.iter().position(|byte| *byte == b'{')?;
    if literal[open + 1..].contains(&b'{') {
        return None;
    }
    let suffix = literal.get(open + 1..)?.strip_prefix(b"}")?;
    (suffix.len() >= 8).then_some(suffix)
}

pub(super) fn write_literal_index(
    path: &Path,
    entries: &[LiteralIndexEntry],
) -> Result<(), String> {
    let mut file = fs::File::create(path)
        .map_err(|error| format!("could not create literal index: {error}"))?;
    file.write_all(LITERAL_INDEX_MAGIC)
        .and_then(|()| file.write_all(&(entries.len() as u64).to_le_bytes()))
        .map_err(|error| format!("could not write literal index: {error}"))?;
    for entry in entries {
        let length = u32::try_from(entry.bytes.len())
            .map_err(|_| "literal index entry is too long".to_owned())?;
        file.write_all(&length.to_le_bytes())
            .and_then(|()| file.write_all(&entry.offset.to_le_bytes()))
            .and_then(|()| file.write_all(&entry.bytes))
            .map_err(|error| format!("could not write literal index: {error}"))?;
    }
    Ok(())
}

pub(super) fn read_literal_index(path: &Path) -> Result<Vec<LiteralIndexEntry>, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("could not open literal index: {error}"))?;
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)
        .map_err(|error| format!("could not read literal index: {error}"))?;
    if &magic != LITERAL_INDEX_MAGIC {
        return Err("Cinder literal index has an unsupported format".to_owned());
    }
    let mut count = [0u8; 8];
    file.read_exact(&mut count)
        .map_err(|error| format!("could not read literal index: {error}"))?;
    let count = usize::try_from(u64::from_le_bytes(count))
        .map_err(|_| "Cinder literal index is too large".to_owned())?;
    let mut entries = Vec::with_capacity(count.min(100_000));
    for _ in 0..count {
        let mut length = [0u8; 4];
        let mut offset = [0u8; 8];
        file.read_exact(&mut length)
            .and_then(|()| file.read_exact(&mut offset))
            .map_err(|error| format!("could not read literal index entry: {error}"))?;
        let length = u32::from_le_bytes(length) as usize;
        if length > 1_048_576 {
            return Err("Cinder literal index entry is too large".to_owned());
        }
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)
            .map_err(|error| format!("could not read literal index entry: {error}"))?;
        entries.push(LiteralIndexEntry {
            bytes,
            offset: u64::from_le_bytes(offset),
        });
    }
    Ok(entries)
}

pub(super) fn snapshot_sources(
    directory: &Path,
    snapshot: &Path,
    sources: &[PathBuf],
) -> Result<(), String> {
    for relative in sources {
        let destination = snapshot.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create source snapshot: {error}"))?;
        }
        fs::copy(directory.join(relative), &destination)
            .map_err(|error| format!("could not snapshot {}: {error}", relative.display()))?;
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct SourceRevisionProbe {
    pub(super) digest: [u8; 32],
    pub(super) metadata: Vec<ArtifactFileIdentity>,
}

impl SourceRevisionProbe {
    pub(super) fn is_current(&self, root: &Path, sources: &[PathBuf]) -> bool {
        self.metadata.len() == sources.len()
            && sources
                .iter()
                .zip(&self.metadata)
                .all(|(relative, expected)| {
                    artifact_file_identity(&root.join(relative)).ok().as_ref() == Some(expected)
                })
    }
}

#[derive(Default)]
pub(super) struct HistoryProbeCache {
    pub(super) sources: BTreeMap<Vec<PathBuf>, SourceRevisionProbe>,
    pub(super) source_probes: usize,
}

impl HistoryProbeCache {
    pub(super) fn source_digest(
        &mut self,
        root: &Path,
        sources: &[PathBuf],
    ) -> Result<[u8; 32], String> {
        if let Some(probe) = self.sources.get(sources) {
            return Ok(probe.digest);
        }
        self.refresh_source_digest(root, sources)
    }

    pub(super) fn refresh_source_digest(
        &mut self,
        root: &Path,
        sources: &[PathBuf],
    ) -> Result<[u8; 32], String> {
        let probe = source_revision_probe(root, sources)?;
        let digest = probe.digest;
        self.sources.insert(sources.to_vec(), probe);
        self.source_probes += 1;
        Ok(digest)
    }

    pub(super) fn source_probe_is_current(&self, root: &Path, sources: &[PathBuf]) -> bool {
        self.sources
            .get(sources)
            .is_some_and(|probe| probe.is_current(root, sources))
    }
}

pub(super) fn source_revision_probe(
    root: &Path,
    sources: &[PathBuf],
) -> Result<SourceRevisionProbe, String> {
    let mut hasher = Sha256::new();
    let mut metadata = Vec::with_capacity(sources.len());
    hasher.update(b"CINDER-SOURCE-REVISION-1");
    for relative in sources {
        let path = root.join(relative);
        let mut file = fs::File::open(&path)
            .map_err(|error| format!("could not open source {}: {error}", path.display()))?;
        let before =
            artifact_file_identity_from_metadata(&file.metadata().map_err(|error| {
                format!("could not inspect source {}: {error}", path.display())
            })?)?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .map_err(|error| format!("could not read source {}: {error}", path.display()))?;
        let after =
            artifact_file_identity_from_metadata(&file.metadata().map_err(|error| {
                format!("could not inspect source {}: {error}", path.display())
            })?)?;
        if before != after || after != artifact_file_identity(&path)? {
            return Err(format!(
                "source changed while Cinder inspected it: {}",
                path.display()
            ));
        }
        append_context_value(&mut hasher, relative.as_os_str().as_bytes());
        append_context_value(&mut hasher, &contents);
        metadata.push(after);
    }
    Ok(SourceRevisionProbe {
        digest: hasher.finalize().into(),
        metadata,
    })
}

pub(super) fn source_revision_digest(root: &Path, sources: &[PathBuf]) -> Result<[u8; 32], String> {
    source_revision_probe(root, sources).map(|probe| probe.digest)
}

pub(super) struct LiteralChange {
    pub(super) relative: PathBuf,
    pub(super) old: Vec<u8>,
    pub(super) new: Vec<u8>,
    pub(super) new_source: Vec<u8>,
}

pub(super) fn find_literal_change(
    directory: &Path,
    snapshot: &Path,
    sources: &[PathBuf],
) -> Result<Option<LiteralChange>, String> {
    let mut change = None;
    for relative in sources {
        let old = fs::read(snapshot.join(relative)).map_err(|error| {
            format!(
                "could not read source snapshot {}: {error}",
                relative.display()
            )
        })?;
        let new = fs::read(directory.join(relative))
            .map_err(|error| format!("could not read source {}: {error}", relative.display()))?;
        if old == new {
            continue;
        }
        if change.is_some() {
            return Ok(None);
        }
        let Some((old_literal, new_literal)) = changed_plain_literal(&old, &new) else {
            return Ok(None);
        };
        change = Some(LiteralChange {
            relative: relative.clone(),
            old: old_literal,
            new: new_literal,
            new_source: new,
        });
    }
    Ok(change)
}

pub(super) fn sources_are_unchanged(
    directory: &Path,
    snapshot: &Path,
    sources: &[PathBuf],
) -> Result<bool, String> {
    for relative in sources {
        if fs::read(directory.join(relative))
            .map_err(|error| format!("could not read {}: {error}", relative.display()))?
            != fs::read(snapshot.join(relative)).map_err(|error| {
                format!(
                    "could not read source snapshot {}: {error}",
                    relative.display()
                )
            })?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn changed_plain_literal(old: &[u8], new: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if old.len() != new.len() || old == new {
        return None;
    }
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let changed_end = old.len() - suffix;
    let old_literal = source_string_literals(old)
        .into_iter()
        .find(|literal| literal.range.start <= prefix && literal.range.end >= changed_end)?;
    let new_literal = source_string_literals(new)
        .into_iter()
        .find(|literal| literal.range == old_literal.range)?;
    changed_literal_data(old_literal.bytes, new_literal.bytes)
}

pub(super) fn changed_literal_data(old: &[u8], new: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if old.len() != new.len()
        || old == new
        || std::str::from_utf8(old).is_err()
        || std::str::from_utf8(new).is_err()
        || old.len() < 8
    {
        return None;
    }
    if old.contains(&b'{') || old.contains(&b'}') || new.contains(&b'{') || new.contains(&b'}') {
        let (old, new) = changed_format_segment(old, new)?;
        Some((old.to_vec(), new.to_vec()))
    } else {
        Some((old.to_vec(), new.to_vec()))
    }
}
