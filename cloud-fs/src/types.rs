use std::error::Error;
use std::fmt;
use std::net::IpAddr;
use std::cmp::min;
use std::io;
use std::path::Path;
use std::cmp::{Ord, Ordering};

use bytes::Bytes;

use crate::backends::Backend;

pub type Data = Bytes;

#[derive(Clone, Debug)]
pub enum FsErrorType {
    ParseError,
    HostNotSupported,
    Other,
}

#[derive(Clone, Debug)]
pub struct FsError {
    error_type: FsErrorType,
    description: String,
}

impl FsError {
    pub(crate) fn new<S: AsRef<str>>(error_type: FsErrorType, description: S) -> FsError {
        FsError {
            error_type,
            description: description.as_ref().to_owned(),
        }
    }

    pub(crate) fn from_error<E>(error: E) -> FsError
    where
        E: Error + fmt::Display,
    {
        Self::new(FsErrorType::Other, format!("{}", error))
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str(&self.description)
    }
}

impl Error for FsError {}

impl From<io::Error> for FsError {
    fn from(e: io::Error) -> FsError {
        FsError::from_error(e)
    }
}

pub type FsResult<R> = Result<R, FsError>;

#[derive(PartialEq, Clone, Debug)]
enum Prefix {
    VerbatimUNC(String, String),
    VerbatimDisk(u8),
    UNC(String, String),
    Disk(u8),
}

impl Prefix {
    fn is_drive_path(string: &str, start: usize, allow_forward: bool) -> bool {
        let buff = string.as_bytes();

        if buff.len() < start + 3 {
            false
        } else if buff[start].is_ascii_alphabetic() && buff[start + 1] == b':' {
            buff[start + 2] == b'\\' || (allow_forward && buff[start + 2] == b'/')
        } else {
            false
        }
    }

    fn try_parse(path: &str) -> FsResult<Option<(Prefix, usize)>> {
        if path.len() < 3 {
            return Ok(None);
        }

        if path.starts_with("\\\\?\\") {
            if path.starts_with("\\\\?\\UNC\\") {
                let (server, next) = FsPath::find_separator(path, 8, false);
                if next == path.len() {
                    return Err(FsError::new(FsErrorType::ParseError, "Incorrect format for verbatim UNC path."));
                }
                let (share, last) = FsPath::find_separator(path, next + 1, false);
                return Ok(Some((
                    Prefix::VerbatimUNC(server.to_owned(), share.to_owned()),
                    last
                )));
            } else if Prefix::is_drive_path(path, 4, false) {
                if let Some(d) = path.bytes().nth(4) {
                    return Ok(Some((Prefix::VerbatimDisk(d), 6)));
                } else {
                    return Err(FsError::new(FsErrorType::ParseError, "Unexpected failure."));
                }
            } else {
                return Err(FsError::new(FsErrorType::ParseError, "Verbatim prefix did not match any supported form."));
            }
        }

        if Prefix::is_drive_path(path, 0, true) {
            return Ok(Some((
                Prefix::Disk(path.as_bytes()[0]), 2
            )));
        }

        if FsPath::find_separator(path, 0, true) == ("", 0) &&
            FsPath::find_separator(path, 1, true) == ("", 1) {
            // Starts with two separators.
            let (server, next) = FsPath::find_separator(path, 2, true);
            if next < path.len() {
                let (share, last) = FsPath::find_separator(path, next + 1, true);
                return Ok(Some((
                    Prefix::UNC(server.to_owned(), share.to_owned()),
                    last
                )));
            }
        }

        Ok(None)
    }

    fn allows_forward_slash(&self) -> bool {
        match self {
            Prefix::VerbatimUNC(_, _) => false,
            Prefix::VerbatimDisk(_) => false,
            _ => true,
        }
    }
}

impl fmt::Display for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            // Good comparisons for all the UNC cases.
            Prefix::VerbatimUNC(ref server, ref share) => f.write_fmt(format_args!("\\\\?\\UNC\\{}\\{}", server, share)),
            Prefix::VerbatimDisk(c) => f.write_fmt(format_args!("\\\\?\\{}:", char::from(*c))),
            Prefix::UNC(ref server, ref share) => f.write_fmt(format_args!("\\\\{}\\{}", server, share)),
            Prefix::Disk(c) => f.write_fmt(format_args!("{}:", char::from(*c))),
        }
    }
}

impl Eq for Prefix { }

impl PartialOrd for Prefix {
    fn partial_cmp(&self, other: &Prefix) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn unc_compare(server_a: &str, share_a: &str, server_b: &str, share_b: &str) -> Ordering {
    let ord = server_a.cmp(server_b);
    if ord != Ordering::Equal {
        return ord;
    }
    share_a.cmp(share_b)
}

impl Ord for Prefix {
    fn cmp(&self, other: &Prefix) -> Ordering {
        if self == other {
            return Ordering::Equal;
        }

        match (self, other) {
            // Good comparisons for the disk cases.
            (Prefix::VerbatimDisk(a), Prefix::VerbatimDisk(b)) => a.cmp(b),
            (Prefix::Disk(a), Prefix::Disk(b)) => a.cmp(b),
            (Prefix::VerbatimDisk(a), Prefix::Disk(b)) => a.cmp(b),

            // Good comparisons for the UNC cases.
            (Prefix::VerbatimUNC(server_a, share_a), Prefix::VerbatimUNC(server_b, share_b)) => unc_compare(server_a, share_a, server_b, share_b),
            (Prefix::UNC(server_a, share_a), Prefix::UNC(server_b, share_b)) => unc_compare(server_a, share_a, server_b, share_b),
            (Prefix::VerbatimUNC(server_a, share_a), Prefix::UNC(server_b, share_b)) => unc_compare(server_a, share_a, server_b, share_b),

            // Now the questionable cases.
            (Prefix::Disk(_), Prefix::VerbatimUNC(_, _)) => Ordering::Less,
            (Prefix::Disk(_), Prefix::UNC(_, _)) => Ordering::Less,
            (Prefix::VerbatimDisk(_), Prefix::VerbatimUNC(_, _)) => Ordering::Less,
            (Prefix::VerbatimDisk(_), Prefix::UNC(_, _)) => Ordering::Less,

            _ => other.cmp(self),
        }
    }
}

#[derive(PartialEq, Clone, Debug)]
enum Component {
    Prefix(Prefix),
    RootDir,
    CurrentDir,
    ParentDir,
    DirMarker,
    PathPart(String),
}

impl Eq for Component { }

impl PartialOrd for Component {
    fn partial_cmp(&self, other: &Component) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Component {
    fn cmp(&self, other: &Component) -> Ordering {
        if self == other {
            return Ordering::Equal;
        }

        match (self, other) {
            // The order of these case doesn't make much sense, but here we go!
            (Component::Prefix(a), Component::Prefix(b)) => a.cmp(b),
            (Component::Prefix(_), _) => Ordering::Greater,
            (Component::RootDir, _) => Ordering::Less,

            // A DirMarker should be the end of the path, so anything else
            // implies that the path is longer.
            (Component::DirMarker, _) => Ordering::Less,

            // Easy peasy!
            (Component::PathPart(a), Component::PathPart(b)) => a.cmp(b),

            // Uhhhhh
            (Component::ParentDir, _) => Ordering::Less,
            (Component::CurrentDir, _) => Ordering::Less,

            (a, b) => b.cmp(a).reverse(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FsPath {
    components: Vec<Component>,
}

impl FsPath {
    fn find_separator(string: &str, start: usize, allow_forward: bool) -> (&str, usize) {
        let part = &string[start..];
        let mut pos = part.find('\\').unwrap_or_else(|| part.len());
        if allow_forward {
            pos = min(pos, part.find('/').unwrap_or_else(|| part.len()));
        }

        if pos == part.len() {
            // Separator not found.
            (&part[..], string.len())
        } else {
            // Separator found. Return the position of the separator character.
            (&part[..pos], start + pos)
        }
    }

    pub fn from_std_path(path: &Path) -> FsResult<FsPath> {
        if let Some(string) = path.to_str() {
            FsPath::new(string)
        } else {
            Err(FsError::new(FsErrorType::ParseError, "Path was not valid utf8."))
        }
    }

    pub fn new<S: AsRef<str>>(from: S) -> FsResult<FsPath> {
        let path = from.as_ref();
        let mut pos: usize = 0;
        let mut result: FsPath = Default::default();
        let mut any_separator = true;

        if let Some((prefix, len)) = Prefix::try_parse(path)? {
            if !prefix.allows_forward_slash() {
                any_separator = false;
            }

            result.components.push(Component::Prefix(prefix));
            pos = len;
        }

        if pos == path.len() {
            return Ok(result);
        }

        let mut first = true;
        loop {
            let (part, next) = FsPath::find_separator(path, pos, any_separator);
            if part == "" {
                if first {
                    result.components.push(Component::RootDir);
                }
            } else if part == ".." {
                result.components.push(Component::ParentDir);
            } else if part == "." {
                result.components.push(Component::CurrentDir);
            } else {
                result.components.push(Component::PathPart(part.to_owned()));
            }

            if !first && next == path.len() - 1 {
                result.components.push(Component::DirMarker);
            }

            if next == path.len() {
                break;
            }

            pos = next + 1;
            first = false;
        }

        Ok(result)
    }

    pub fn is_absolute(&self) -> bool {
        if let Some(first) = self.components.first() {
            match first {
                Component::Prefix(_) => true,
                Component::RootDir => true,
                _ => false,
            }
        } else {
            false
        }
    }

    pub fn is_directory(&self) -> bool {
        if let Some(last) = self.components.last() {
            match last {
                Component::Prefix(_) => true,
                Component::RootDir => true,
                Component::DirMarker => true,
                _ => false
            }
        } else {
            true
        }
    }

    pub fn join<P: AsRef<FsPath>>(&self, path: P) -> FsPath {
        unimplemented!();
    }
}

impl fmt::Display for FsPath {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let separator = if let Some(Component::Prefix(_)) = self.components.get(0) {
            "\\"
        } else {
            "/"
        };

        let mut needs_separator = false;
        for component in &self.components {
            match component {
                Component::Prefix(p) => p.fmt(f)?,
                Component::RootDir => f.write_str(separator)?,
                Component::DirMarker => f.write_str(separator)?,
                _ => {
                    if needs_separator {
                        f.write_str(separator)?;
                    }
                    match component {
                        Component::CurrentDir => f.write_str(".")?,
                        Component::ParentDir => f.write_str("..")?,
                        Component::PathPart(s) => f.write_str(&s)?,
                        _ => unreachable!(),
                    }
                    needs_separator = true;
                }
            }
        }

        Ok(())
    }
}

impl Eq for FsPath { }

impl PartialOrd for FsPath {
    fn partial_cmp(&self, other: &FsPath) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FsPath {
    fn cmp(&self, other: &FsPath) -> Ordering {
        if self.is_absolute() != other.is_absolute() {
            // Weird case.
            if self.is_absolute() {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        } else {
            for i in 0..min(self.components.len(), other.components.len()) {
                let ord = self.components[i].cmp(&other.components[i]);
                if ord != Ordering::Equal {
                    return ord;
                }
            }

            if self.components.len() < other.components.len() {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
    }
}

impl AsRef<FsPath> for FsPath {
    fn as_ref(&self) -> &FsPath {
        self
    }
}

#[derive(Clone, Debug)]
pub enum FsHost {
    HostName(String),
    Address(IpAddr),
}

#[derive(Clone, Debug)]
struct FsTarget {
    host: FsHost,
    port: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct FsSettings {
    backend: Backend,
    target: Option<FsTarget>,
    path: FsPath,
}

impl FsSettings {
    pub fn new(backend: Backend, path: FsPath) -> FsSettings {
        FsSettings {
            backend,
            target: None,
            path,
        }
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    pub fn hostname(&self) -> Option<&FsHost> {
        self.target.as_ref().map(|h| &h.host)
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct FsFile {
    path: FsPath,
    size: Option<usize>,
}

impl FsFile {
    pub fn new(path: FsPath, size: Option<usize>) -> FsFile {
        FsFile { path, size, }
    }
}

impl Eq for FsFile { }

impl PartialOrd for FsFile {
    fn partial_cmp(&self, other: &FsFile) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FsFile {
    fn cmp(&self, other: &FsFile) -> Ordering {
        self.path.cmp(&other.path)
    }
}

#[cfg(test)]
#[allow(clippy::cognitive_complexity)]
mod tests {
    use super::*;

    #[test]
    fn test_path_parse_basic() -> FsResult<()> {
        let path = FsPath::new("/foo/bar")?;
        assert_eq!(path, FsPath {
            components: vec![
                Component::RootDir,
                Component::PathPart(String::from("foo")),
                Component::PathPart(String::from("bar")),
            ],
        });
        assert_eq!(path.to_string(), "/foo/bar");
        assert!(path.is_absolute());
        assert!(!path.is_directory());

        let path = FsPath::new("foo/bar")?;
        assert_eq!(path, FsPath {
            components: vec![
                Component::PathPart(String::from("foo")),
                Component::PathPart(String::from("bar")),
            ],
        });
        assert_eq!(path.to_string(), "foo/bar");
        assert!(!path.is_absolute());
        assert!(!path.is_directory());

        let path = FsPath::new("foo/bar/")?;
        assert_eq!(path, FsPath {
            components: vec![
                Component::PathPart(String::from("foo")),
                Component::PathPart(String::from("bar")),
                Component::DirMarker,
            ],
        });
        assert_eq!(path.to_string(), "foo/bar/");
        assert!(!path.is_absolute());
        assert!(path.is_directory());

        let path = FsPath::new("/")?;
        assert_eq!(path, FsPath {
            components: vec![
                Component::RootDir,
            ],
        });
        assert_eq!(path.to_string(), "/");
        assert!(path.is_absolute());
        assert!(path.is_directory());

        let path = FsPath::new("")?;
        assert_eq!(path, FsPath {
            components: vec![
            ],
        });
        assert_eq!(path.to_string(), "");
        assert!(!path.is_absolute());
        assert!(path.is_directory());

        let path = FsPath::new("foo\\bar/")?;
        assert_eq!(path, FsPath {
            components: vec![
                Component::PathPart(String::from("foo")),
                Component::PathPart(String::from("bar")),
                Component::DirMarker,
            ],
        });
        assert_eq!(path.to_string(), "foo/bar/");
        assert!(!path.is_absolute());
        assert!(path.is_directory());

        let path = FsPath::new("\\foo\\bar")?;
        assert_eq!(path, FsPath {
            components: vec![
                Component::RootDir,
                Component::PathPart(String::from("foo")),
                Component::PathPart(String::from("bar")),
            ],
        });
        assert_eq!(path.to_string(), "/foo/bar");
        assert!(path.is_absolute());
        assert!(!path.is_directory());

        Ok(())
    }

    #[test]
    fn test_path_parse_windows() -> FsResult<()> {
        let path = FsPath::new("C:\\foo\\bar")?;
        assert_eq!(path, FsPath {
            components: vec![
                Component::Prefix(Prefix::Disk(b'C')),
                Component::RootDir,
                Component::PathPart(String::from("foo")),
                Component::PathPart(String::from("bar")),
            ],
        });
        assert_eq!(path.to_string(), "C:\\foo\\bar");
        assert!(path.is_absolute());
        assert!(!path.is_directory());

        let path = FsPath::new("C:/foo\\bar")?;
        assert_eq!(path, FsPath {
            components: vec![
                Component::Prefix(Prefix::Disk(b'C')),
                Component::RootDir,
                Component::PathPart(String::from("foo")),
                Component::PathPart(String::from("bar")),
            ],
        });
        assert_eq!(path.to_string(), "C:\\foo\\bar");
        assert!(path.is_absolute());
        assert!(!path.is_directory());

        let path = FsPath::new("\\\\bar\\foo/test")?;
        assert_eq!(path, FsPath {
            components: vec![
                Component::Prefix(Prefix::UNC(String::from("bar"), String::from("foo"))),
                Component::RootDir,
                Component::PathPart(String::from("test")),
            ],
        });
        assert_eq!(path.to_string(), "\\\\bar\\foo\\test");
        assert!(path.is_absolute());
        assert!(!path.is_directory());

        let path = FsPath::new("\\\\?\\C:\\foo\\bar")?;
        assert_eq!(path, FsPath {
            components: vec![
                Component::Prefix(Prefix::VerbatimDisk(b'C')),
                Component::RootDir,
                Component::PathPart(String::from("foo")),
                Component::PathPart(String::from("bar")),
            ],
        });
        assert_eq!(path.to_string(), "\\\\?\\C:\\foo\\bar");
        assert!(path.is_absolute());
        assert!(!path.is_directory());

        let path = FsPath::new("\\\\?\\C:\\foo/bar")?;
        assert_eq!(path, FsPath {
            components: vec![
                Component::Prefix(Prefix::VerbatimDisk(b'C')),
                Component::RootDir,
                Component::PathPart(String::from("foo/bar")),
            ],
        });
        assert_eq!(path.to_string(), "\\\\?\\C:\\foo/bar");
        assert!(path.is_absolute());
        assert!(!path.is_directory());

        let path = FsPath::new("\\\\?\\UNC\\bar\\foo\\test")?;
        assert_eq!(path, FsPath {
            components: vec![
                Component::Prefix(Prefix::VerbatimUNC(String::from("bar"), String::from("foo"))),
                Component::RootDir,
                Component::PathPart(String::from("test")),
            ],
        });
        assert_eq!(path.to_string(), "\\\\?\\UNC\\bar\\foo\\test");
        assert!(path.is_absolute());
        assert!(!path.is_directory());

        Ok(())
    }
}
