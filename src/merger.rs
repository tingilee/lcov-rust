use super::{Record, RecordKind};
use std::collections::BTreeMap;
use std::iter;
use std::path::PathBuf;

#[derive(Debug, Clone, Fail, Eq, PartialEq)]
pub enum Error<ReadError> {
    #[fail(display = "failed to read record: {}", _0)] Read(#[cause] ReadError),
    #[fail(display = "unexpected record `{}`", _0)] UnexpectedRecord(RecordKind),
    #[fail(display = "unexpected end of stream")] UnexpectedEof,
    #[fail(display = "unmatched function line")] UnmatchedFunctionLine,
    #[fail(display = "unmatches checksum")] UnmatchedChecksum,
}

macro_rules! eat {
    ($parser:expr, $p:pat) => { eat!($parser, $p => {}) };
    ($parser:expr, $p:pat => $body:expr) => {
        match $parser.pop().map_err(Error::Read)? {
            Some($p) => $body,
            Some(rec) => Err(Error::UnexpectedRecord(rec.kind()))?,
            None => Err(Error::UnexpectedEof)?,
        }
    }
}

macro_rules! eat_if_matches {
    ($parser:expr, $p:pat) => { eat_if_matches!($parser, $p => {}) };
    ($parser:expr, $p:pat => $body:expr) => {
        match $parser.pop().map_err(Error::Read)? {
            Some($p)=>Some($body),
            Some(item) => {
                $parser.push(item);
                None
            }
            None => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Parser<I, T> {
    iter: I,
    next_item: Option<T>,
}

impl<I, T, E> Parser<I, T>
where
    I: Iterator<Item = Result<T, E>>,
{
    fn new(iter: I) -> Self {
        Parser {
            iter,
            next_item: None,
        }
    }

    fn push(&mut self, item: T) {
        assert!(self.next_item.is_none());
        self.next_item = Some(item);
    }

    fn pop(&mut self) -> Result<Option<T>, E> {
        if let Some(next) = self.next_item.take() {
            return Ok(Some(next));
        }
        if let Some(item) = self.iter.next() {
            item.map(Some)
        } else {
            Ok(None)
        }
    }

    fn peek(&mut self) -> Result<Option<&T>, E> {
        if let Some(ref next) = self.next_item {
            return Ok(Some(next));
        }
        self.next_item = if let Some(item) = self.iter.next() {
            Some(item?)
        } else {
            None
        };
        Ok(self.next_item.as_ref())
    }
}


#[derive(Debug, Clone, Default)]
pub struct Merger {
    files: BTreeMap<FileKey, File>,
}

impl Merger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn merge<I, E>(&mut self, it: I) -> Result<(), Error<E>>
    where
        I: IntoIterator<Item = Result<Record, E>>,
    {
        let mut parser = Parser::new(it.into_iter());

        while let Some(_) = parser.peek().map_err(Error::Read)? {
            let test_name =
                eat_if_matches!(parser, Record::TestName { name } => name).unwrap_or("".into());
            let source_file = eat!(parser, Record::SourceFile { path } => path);
            let key = FileKey {
                test_name,
                source_file,
            };
            let file = self.files.entry(key).or_insert_with(Default::default);
            file.merge(&mut parser)?;
            eat!(parser, Record::EndOfRecord);
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Default, Ord, PartialOrd, Eq, PartialEq, Hash)]
struct FileKey {
    test_name: String,
    source_file: PathBuf,
}

#[derive(Debug, Clone, Default)]
struct File {
    fn_lines: BTreeMap<String, u32>,
    fn_data: BTreeMap<String, u64>,
    br_data: BTreeMap<BranchKey, Option<u64>>,
    ln_data: BTreeMap<u32, LineData>,
}

impl File {
    fn merge<I, E>(&mut self, parser: &mut Parser<I, Record>) -> Result<(), Error<E>>
    where
        I: Iterator<Item = Result<Record, E>>,
    {
        // FunctionName
        while let Some((name, start_line)) =
            eat_if_matches!(parser, Record::FunctionName { name, start_line } => (name, start_line))
        {
            let line = *self.fn_lines.entry(name).or_insert(start_line);
            if line != start_line {
                Err(Error::UnmatchedFunctionLine)?;
            }
        }

        // FunctionData
        while let Some((name, count)) =
            eat_if_matches!(parser, Record::FunctionData { name, count } => { (name, count) })
        {
            *self.fn_data.entry(name).or_insert(0) += count;
        }

        eat_if_matches!(parser, Record::FunctionsFound { .. });
        eat_if_matches!(parser, Record::FunctionsHit { .. });

        // BranchData
        while let Some((key, taken)) = eat_if_matches!(parser,
            Record::BranchData { line, block, branch, taken } => {
                (BranchKey { line, block, branch }, taken)
            }
        ) {
            let org = self.br_data.entry(key).or_insert(None);
            if let Some(taken) = taken {
                *org = Some(org.unwrap_or(0) + taken);
            }
        }

        eat_if_matches!(parser, Record::BranchesFound { .. });
        eat_if_matches!(parser, Record::BranchesHit { .. });

        // LineData
        while let Some((line, count, checksum)) = eat_if_matches!(parser,
            Record::LineData { line, count, checksum } => {
                (line, count, checksum)
            }
        ) {
            let org = self.ln_data.entry(line).or_insert(LineData::default());
            org.count += count;
            if let Some(checksum) = checksum {
                if let Some(org_checksum) = org.checksum.as_ref() {
                    if checksum != *org_checksum {
                        Err(Error::UnmatchedChecksum)?;
                    }
                }
                org.checksum = Some(checksum);
            }
        }

        eat_if_matches!(parser, Record::LinesFound { .. });
        eat_if_matches!(parser, Record::LinesHit { .. });

        Ok(())
    }
}

#[derive(Debug, Clone, Default, Hash, Ord, PartialOrd, Eq, PartialEq)]
struct BranchKey {
    line: u32,
    block: u32,
    branch: u32,
}

#[derive(Debug, Clone, Default)]
struct LineData {
    count: u64,
    checksum: Option<String>,
}

impl IntoIterator for Merger {
    type Item = Record;
    type IntoIter = IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            inner: Box::new(self.files.into_iter().flat_map(|(key, file)| {
                iter::once(Record::TestName {
                    name: key.test_name,
                }).chain(iter::once(Record::SourceFile {
                    path: key.source_file,
                }))
                    .chain(file.into_iter())
                    .chain(iter::once(Record::EndOfRecord))
            })),
        }
    }
}

pub struct IntoIter {
    inner: Box<Iterator<Item = Record>>,
}

impl Iterator for IntoIter {
    type Item = Record;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

impl IntoIterator for File {
    type Item = Record;
    type IntoIter = FileIntoIter;

    fn into_iter(self) -> Self::IntoIter {
        enum Func {
            Line((String, u32)),
            Data((String, u64)),
            Found(bool, u32),
            Hit(bool, u32),
        }
        let fn_line = self.fn_lines.into_iter().map(Func::Line);
        let fn_data = self.fn_data.into_iter().map(Func::Data);
        let fn_iter = fn_line
            .chain(fn_data)
            .chain(iter::once(Func::Found(false, 0)))
            .chain(iter::once(Func::Hit(false, 0)))
            .scan((0, 0), |st, mut rec| {
                let do_emit = st.0 > 0;
                match &mut rec {
                    &mut Func::Line(..) => st.0 += 1,
                    &mut Func::Data((_, ref mut count)) if *count > 0 => st.1 += 1,
                    &mut Func::Data(..) => {}
                    &mut Func::Found(ref mut emit, ref mut count) => {
                        *emit = do_emit;
                        *count = st.0
                    }
                    &mut Func::Hit(ref mut emit, ref mut hit) => {
                        *emit = do_emit;
                        *hit = st.1
                    }
                }
                Some(rec)
            })
            .filter_map(|rec| match rec {
                Func::Line((name, start_line)) => Some(Record::FunctionName { name, start_line }),
                Func::Data((name, count)) => Some(Record::FunctionData { name, count }),
                Func::Found(true, found) => Some(Record::FunctionsFound { found }),
                Func::Found(false, _) => None,
                Func::Hit(true, hit) => Some(Record::FunctionsHit { hit }),
                Func::Hit(false, _) => None,
            });

        enum Branch {
            Data((BranchKey, Option<u64>)),
            Found(bool, u32),
            Hit(bool, u32),
        }
        let branch_iter = self.br_data
            .into_iter()
            .map(Branch::Data)
            .chain(iter::once(Branch::Found(false, 0)))
            .chain(iter::once(Branch::Hit(false, 0)))
            .scan((0, 0), |st, mut rec| {
                let do_emit = st.0 > 0;
                debug_assert!(st.0 >= st.1);
                match &mut rec {
                    &mut Branch::Data((_, taken)) => {
                        st.0 += 1;
                        if taken.unwrap_or(0) > 0 {
                            st.1 += 1;
                        }
                    }
                    &mut Branch::Found(ref mut emit, ref mut found) => {
                        *emit = do_emit;
                        *found = st.0;
                    }
                    &mut Branch::Hit(ref mut emit, ref mut hit) => {
                        *emit = do_emit;
                        *hit = st.1;
                    }
                }
                Some(rec)
            })
            .filter_map(|rec| match rec {
                Branch::Data((key, taken)) => Some(Record::BranchData {
                    line: key.line,
                    block: key.block,
                    branch: key.branch,
                    taken: taken,
                }),
                Branch::Found(true, found) => Some(Record::BranchesFound { found }),
                Branch::Found(false, _) => None,
                Branch::Hit(true, hit) => Some(Record::BranchesHit { hit }),
                Branch::Hit(false, _) => None,
            });

        enum Line {
            Data((u32, LineData)),
            Found(u32),
            Hit(u32),
        }
        let line_iter = self.ln_data
            .into_iter()
            .map(Line::Data)
            .chain(iter::once(Line::Found(0)))
            .chain(iter::once(Line::Hit(0)))
            .scan((0, 0), |st, mut rec| {
                match &mut rec {
                    &mut Line::Data((_, ref data)) => {
                        st.0 += 1;
                        if data.count > 0 {
                            st.1 += 1;
                        }
                    }
                    &mut Line::Found(ref mut found) => *found = st.0,
                    &mut Line::Hit(ref mut hit) => *hit = st.1,
                };
                Some(rec)
            })
            .map(|rec| match rec {
                Line::Data((line, data)) => Record::LineData {
                    line,
                    count: data.count,
                    checksum: data.checksum,
                },
                Line::Found(found) => Record::LinesFound { found },
                Line::Hit(hit) => Record::LinesHit { hit },
            });

        let iter = fn_iter.chain(branch_iter).chain(line_iter);
        FileIntoIter {
            inner: Box::new(iter),
        }
    }
}

struct FileIntoIter {
    inner: Box<Iterator<Item = Record>>,
}

impl Iterator for FileIntoIter {
    type Item = Record;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}
