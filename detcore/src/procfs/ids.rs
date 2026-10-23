/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The process and thread numbers a `/proc` path names, and the same path
//! with those numbers translated between the guest's numbering (Detcore's)
//! and the kernel's. They differ on DBT, whose kernel numbers the container's
//! processes otherwise than Detcore does (slice B3a-4 of
//! <https://github.com/rrnewton/hermit/issues/4266>): there `/proc/3`, the
//! guest's own directory, is the kernel's `/proc/4`, and the kernel's `/proc/3`
//! is no guest process at all.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

/// A process or thread number as `/proc` spells it: decimal digits with no
/// leading zero (Linux names nothing `03`).
fn number(name: &[u8]) -> Option<i32> {
    if name.first() == Some(&b'0') || !name.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(name).ok()?.parse().ok()
}

/// Where a lookup is, walking a path's components: what the components so
/// far name, as far as the numbers below `/proc` go.
#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    Root,
    /// `/proc` itself.
    Proc,
    /// A process's directory; `init` for the namespace's init (`/proc/1`).
    Process {
        init: bool,
    },
    /// A process's `task`.
    Tasks {
        init: bool,
    },
    /// A thread's directory.
    Thread,
    /// A directory of links (`fd`, `ns`, `map_files`).
    Links,
    /// Past a link that leads anywhere (`cwd`, `root`, `exe`, `fd/N`): where
    /// the lookup is depends on where the link leads.
    Opaque,
    /// A directory outside `/proc`; `resolved` where it is known to be no
    /// link (a component of a resolved directory), so that its `..` is its
    /// lexical parent and its entries are no process's.
    Elsewhere {
        resolved: bool,
    },
    /// Anything else inside `/proc`, which has no other links: no numbers
    /// below.
    Other,
}

/// One number a path names: its position among the path's components (see
/// [`components`]), the number, and whether it is a thread of the
/// namespace's init (`/proc/1/task/<T>`), Hermit's own, which keeps its
/// number in both numberings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Named {
    position: usize,
    id: i32,
    init_thread: bool,
}

/// `path`'s components after the root, every `.` and `..` included, so
/// that a path rebuilt from them resolves as the original does. Split on the
/// bytes: `Path::components` drops a `.` that is not first, which changes
/// how a lookup ends (`stat/.` is ENOTDIR, `cwd/.` follows the link).
pub(crate) fn components(path: &Path) -> Vec<OsString> {
    path.as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .filter(|name| !name.is_empty())
        .map(|name| OsString::from(OsStr::from_bytes(name)))
        .collect()
}

/// A walk of an absolute path's components (see [`walk`]).
struct Walk {
    /// The numbers named before `unresolved`.
    named: Vec<Named>,
    /// The position of the first component whose meaning depends on where a
    /// link before it leads: a number, or a `..` a number follows, past a
    /// link, or past a directory outside `/proc` that may be one. The walk
    /// stops there.
    unresolved: Option<usize>,
    /// Whether the path, walked to its end, is the namespace's init's
    /// directory or below it.
    init: bool,
}

/// Walk the absolute path `path`'s components as a lookup does, for the
/// numbers it names as a process (an entry of `/proc`) or a thread (an
/// entry of a process's `task`). `..` goes back to the directory it left,
/// also from `self` (to `/proc`), `thread-self` (to the process's `task`)
/// and `/proc/net` (`self/net`). The first `resolved` components are known
/// to be no links. The walk stops where the meaning of a component depends
/// on where a link before it leads ([`Walk::unresolved`]).
fn walk(path: &Path, resolved: usize) -> Walk {
    let mut named = Vec::new();
    let mut stack = vec![At::Root];
    if !path.is_absolute() {
        return Walk {
            named,
            unresolved: None,
            init: false,
        };
    }
    let names = components(path);
    // A `..` whose meaning a link decides matters only where a number
    // follows: nothing else is translated.
    let last_number = names
        .iter()
        .rposition(|name| number(name.as_bytes()).is_some());
    for (position, name) in names.iter().enumerate() {
        let top = *stack.last().expect("the walk starts at the root");
        let name = name.as_bytes();
        let unresolved = matches!(top, At::Opaque | At::Elsewhere { resolved: false })
            && (number(name).is_some()
                || (name == b".." && last_number.is_some_and(|last| last > position)));
        if unresolved {
            return Walk {
                named,
                unresolved: Some(position),
                init: false,
            };
        }
        match name {
            b"." => continue,
            b".." => {
                if stack.len() > 1 {
                    stack.pop();
                }
                continue;
            }
            _ => {}
        }
        let elsewhere = At::Elsewhere {
            resolved: position < resolved,
        };
        match top {
            At::Root if name == b"proc" => stack.push(At::Proc),
            At::Root | At::Elsewhere { .. } => stack.push(elsewhere),
            At::Proc => match name {
                b"self" => stack.push(At::Process { init: false }),
                b"thread-self" => stack.extend([
                    At::Process { init: false },
                    At::Tasks { init: false },
                    At::Thread,
                ]),
                b"net" => stack.extend([At::Process { init: false }, At::Other]),
                _ => match number(name) {
                    Some(id) => {
                        named.push(Named {
                            position,
                            id,
                            init_thread: false,
                        });
                        stack.push(At::Process { init: id == 1 });
                    }
                    None => stack.push(At::Other),
                },
            },
            At::Process { init } if name == b"task" => stack.push(At::Tasks { init }),
            At::Process { .. } | At::Thread => match name {
                b"fd" | b"ns" | b"map_files" => stack.push(At::Links),
                b"cwd" | b"root" | b"exe" => stack.push(At::Opaque),
                _ => stack.push(At::Other),
            },
            At::Tasks { init } => match number(name) {
                Some(id) => {
                    named.push(Named {
                        position,
                        id,
                        init_thread: init,
                    });
                    stack.push(At::Thread);
                }
                None => stack.push(At::Other),
            },
            At::Links | At::Opaque => stack.push(At::Opaque),
            At::Other => stack.push(At::Other),
        }
    }
    Walk {
        named,
        unresolved: None,
        init: stack.contains(&At::Process { init: true }),
    }
}

/// The process and thread numbers the absolute path `path` names that the
/// two numberings may number otherwise (see [`walk`]): every one before the
/// walk stops but the threads of the namespace's init.
pub(crate) fn named_ids(path: &Path) -> Vec<i32> {
    walk(path, 0)
        .named
        .into_iter()
        .filter(|named| !named.init_thread)
        .map(|named| named.id)
        .collect()
}

/// `names` as a path, absolute or relative, with the trailing slash of
/// `like` where `trailing`.
fn rebuilt(names: Vec<OsString>, absolute: bool, like: &Path, trailing: bool) -> PathBuf {
    let mut rebuilt = if absolute {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    };
    rebuilt.extend(names);
    if trailing && like.as_os_str().as_bytes().ends_with(b"/") {
        rebuilt.as_mut_os_string().push("/");
    }
    rebuilt
}

/// `names` as a path like `like`: absolute where it is, with its trailing
/// slash.
pub(crate) fn rebuilt_like(names: Vec<OsString>, like: &Path) -> PathBuf {
    rebuilt(names, like.is_absolute(), like, true)
}

/// The lookup of `rest`, absolute, or relative to the resolved directory
/// `base` (absolute, with no link among its components), as far as the
/// numbers `/proc` names go.
pub(crate) struct Lookup<'a> {
    rest: &'a Path,
    first: usize,
    walk: Walk,
}

impl<'a> Lookup<'a> {
    /// Walk `rest` from `base` (see [`walk`]). An absolute `rest` ignores
    /// `base`.
    pub(crate) fn new(base: Option<&Path>, rest: &'a Path) -> Self {
        let base = base.filter(|_| !rest.is_absolute());
        let (first, walk) = match base {
            None => (0, walk(rest, 0)),
            Some(base) => {
                let first = components(base).len();
                (first, walk(&base.join(rest), first))
            }
        };
        Lookup { rest, first, walk }
    }

    /// The numbers within `rest` that the two numberings may number
    /// otherwise: every one before the walk stops but the threads of the
    /// namespace's init. By position: a number in `base` is not `rest`'s.
    pub(crate) fn ids(&self) -> Vec<i32> {
        self.translatable().map(|named| named.id).collect()
    }

    fn translatable(&self) -> impl Iterator<Item = &Named> {
        self.walk
            .named
            .iter()
            .filter(|named| named.position >= self.first && !named.init_thread)
    }

    /// Each component of `rest` (by its position among [`components`] of
    /// `rest`) that is a number [`Self::ids`] finds and `map` changes, and
    /// what it becomes: `map` of it, or `0` where `map` has none, so that a
    /// lookup fails as it does for a number that names no process.
    pub(crate) fn translations(&self, map: impl Fn(i32) -> Option<i32>) -> Vec<(usize, OsString)> {
        self.translatable()
            .filter_map(|named| {
                let translated = map(named.id).unwrap_or(0);
                (translated != named.id).then(|| {
                    (
                        named.position - self.first,
                        OsString::from(translated.to_string()),
                    )
                })
            })
            .collect()
    }

    /// `rest` with its [`Self::translations`]. Every other component, `.`
    /// and `..` included, is kept, so the kernel resolves the path as it
    /// would the guest's.
    pub(crate) fn respelled(&self, map: impl Fn(i32) -> Option<i32>) -> PathBuf {
        let mut names = components(self.rest);
        for (position, name) in self.translations(map) {
            names[position] = name;
        }
        rebuilt_like(names, self.rest)
    }

    /// Where the walk stops (see [`Walk::unresolved`]), by position among
    /// [`components`] of `rest`: the directory the components before it
    /// name decides what the rest means.
    pub(crate) fn stop(&self) -> Option<usize> {
        Some(self.walk.unresolved? - self.first)
    }
}

/// The absolute path `path` with each number it names (see [`named_ids`])
/// replaced (see [`Lookup::translations`]). `None` when nothing changes,
/// which includes every path that names no number.
pub(crate) fn translate(path: &Path, map: impl Fn(i32) -> Option<i32>) -> Option<PathBuf> {
    let lookup = Lookup::new(None, path);
    let respelled = lookup.respelled(&map);
    (!lookup.translations(map).is_empty()).then_some(respelled)
}

/// Whether the absolute path `path`, walked to its end, is the namespace's
/// init's directory or below it (`/proc/1/...`): Hermit's own process, whose
/// numbers are the same in both numberings.
pub(crate) fn names_init(path: &Path) -> bool {
    walk(path, 0).init
}

/// Whether `name`, an entry of the `/proc` directory spelled `directory`
/// (resolved), is a process or thread number: an entry of `/proc` itself or
/// of a `task` directory.
pub(crate) fn names_an_id(directory: &Path, name: &OsStr) -> bool {
    !Lookup::new(Some(directory), Path::new(name))
        .ids()
        .is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel(id: i32) -> Option<i32> {
        match id {
            1 => Some(1),
            3 => Some(4),
            5 => Some(9),
            _ => None,
        }
    }

    #[test]
    fn named_ids_are_the_process_and_its_task() {
        assert_eq!(named_ids(Path::new("/proc/3/stat")), [3]);
        assert_eq!(named_ids(Path::new("/proc/3/task/5/stat")), [3, 5]);
        assert_eq!(named_ids(Path::new("/proc/self/task/5")), [5]);
        // A thread's directory has no `task`.
        assert!(named_ids(Path::new("/proc/thread-self/task/5")).is_empty());
        assert!(named_ids(Path::new("/proc/self/stat")).is_empty());
        assert!(named_ids(Path::new("/proc/sys/kernel/pid_max")).is_empty());
        // A leading zero names nothing; nor does a path outside /proc or a
        // relative one.
        assert!(named_ids(Path::new("/proc/03/stat")).is_empty());
        assert!(named_ids(Path::new("/tmp/3")).is_empty());
        assert!(named_ids(Path::new("proc/3")).is_empty());
        // Below the process, only `task/<T>` names a number.
        assert!(named_ids(Path::new("/proc/3/fd/5")).len() == 1);
    }

    #[test]
    fn translation_rewrites_only_the_numbers() {
        assert_eq!(
            translate(Path::new("/proc/3/stat"), kernel),
            Some(PathBuf::from("/proc/4/stat"))
        );
        assert_eq!(
            translate(Path::new("/proc/3/task/5/comm"), kernel),
            Some(PathBuf::from("/proc/4/task/9/comm"))
        );
        assert_eq!(
            translate(Path::new("/proc/self/task/5"), kernel),
            Some(PathBuf::from("/proc/self/task/9"))
        );
        assert_eq!(
            translate(Path::new("/proc/3/"), kernel),
            Some(PathBuf::from("/proc/4/"))
        );
        // The descriptor numbers below `fd` are not process numbers.
        assert_eq!(
            translate(Path::new("/proc/3/fd/5"), kernel),
            Some(PathBuf::from("/proc/4/fd/5"))
        );
        // Unchanged: pid 1 is the same in both, and so are its threads; no
        // number; not under /proc.
        assert_eq!(translate(Path::new("/proc/1/stat"), kernel), None);
        assert_eq!(translate(Path::new("/proc/1/task/2/ns/pid"), kernel), None);
        assert_eq!(translate(Path::new("/proc/self/stat"), kernel), None);
        assert_eq!(translate(Path::new("/proc/uptime"), kernel), None);
        assert_eq!(translate(Path::new("/etc/3"), kernel), None);
    }

    #[test]
    fn a_number_with_no_counterpart_names_no_process() {
        assert_eq!(
            translate(Path::new("/proc/7/stat"), kernel),
            Some(PathBuf::from("/proc/0/stat"))
        );
        assert_eq!(
            translate(Path::new("/proc/3/task/8"), kernel),
            Some(PathBuf::from("/proc/4/task/0"))
        );
    }

    /// The relative `rest`, looked up from the resolved `base`, respelled
    /// where a number changes.
    fn relative(base: &str, rest: &str) -> Option<PathBuf> {
        let lookup = Lookup::new(Some(Path::new(base)), Path::new(rest));
        (!lookup.translations(kernel).is_empty()).then(|| lookup.respelled(kernel))
    }

    /// Where the walk of `rest` stops: the components before the stop,
    /// spelled with the kernel's numbers, and the rest.
    fn unresolved(base: Option<&str>, rest: &str) -> Option<(PathBuf, PathBuf)> {
        let lookup = Lookup::new(base.map(Path::new), Path::new(rest));
        let at = lookup.stop()?;
        let names = components(&lookup.respelled(kernel));
        let directory = rebuilt(
            names[..at].to_vec(),
            rest.starts_with('/'),
            Path::new(rest),
            false,
        );
        let directory = match base {
            Some(base) => Path::new(base).join(directory),
            None => directory,
        };
        Some((
            directory,
            rebuilt(names[at..].to_vec(), false, Path::new(rest), true),
        ))
    }

    fn pair(directory: &str, rest: &str) -> Option<(PathBuf, PathBuf)> {
        Some((PathBuf::from(directory), PathBuf::from(rest)))
    }

    #[test]
    fn dot_dot_goes_back_where_a_lookup_goes() {
        // `..` leaves the process for /proc, so the next number is a
        // process; each component stays, so the kernel resolves the same.
        assert_eq!(named_ids(Path::new("/proc/3/../5/stat")), [3, 5]);
        assert_eq!(
            translate(Path::new("/proc/3/../5/stat"), kernel),
            Some(PathBuf::from("/proc/4/../9/stat"))
        );
        // `self` is a process of /proc, `thread-self` a thread of its task,
        // and `/proc/net` is `self/net`.
        assert_eq!(
            translate(Path::new("/proc/self/../3/stat"), kernel),
            Some(PathBuf::from("/proc/self/../4/stat"))
        );
        assert_eq!(
            translate(Path::new("/proc/thread-self/../5"), kernel),
            Some(PathBuf::from("/proc/thread-self/../9"))
        );
        assert_eq!(
            translate(Path::new("/proc/net/../../3/exe"), kernel),
            Some(PathBuf::from("/proc/net/../../4/exe"))
        );
        // Every `.` is kept, byte for byte.
        let bytes = |path: Option<PathBuf>| path.map(|path| path.into_os_string());
        assert_eq!(
            bytes(translate(Path::new("/proc/3/."), kernel)),
            Some("/proc/4/.".into())
        );
        assert_eq!(
            bytes(translate(Path::new("/proc/3/stat/."), kernel)),
            Some("/proc/4/stat/.".into())
        );
        assert_eq!(
            bytes(translate(Path::new("/proc/3/./cwd/."), kernel)),
            Some("/proc/4/./cwd/.".into())
        );
        assert_eq!(
            bytes(relative("/proc", "5/stat/.")),
            Some("9/stat/.".into())
        );
    }

    #[test]
    fn past_a_link_the_walk_stops_where_the_link_decides() {
        // A `..` or a number past a link that leads anywhere depends on
        // where it leads: the walk stops there, with the directory to resolve
        // spelled with the kernel's numbers.
        assert_eq!(named_ids(Path::new("/proc/3/cwd/../5")), [3]);
        assert_eq!(
            unresolved(None, "/proc/3/cwd/../5"),
            pair("/proc/4/cwd", "../5")
        );
        assert_eq!(named_ids(Path::new("/proc/3/fd/7/../5")), [3]);
        assert_eq!(named_ids(Path::new("/proc/3/task/5/cwd/../../6")), [3, 5]);
        // A descriptor of /proc, by its link.
        assert_eq!(
            unresolved(None, "/proc/self/fd/4/3/exe"),
            pair("/proc/self/fd/4", "3/exe")
        );
        // No number follows, so no matter where the link leads.
        assert_eq!(unresolved(None, "/proc/3/cwd/file"), None);
        assert_eq!(unresolved(None, "/proc/3/cwd/../file"), None);
        assert_eq!(unresolved(None, "/usr/lib/../lib64/ld.so"), None);
        assert_eq!(
            translate(Path::new("/proc/3/fd/7/x"), kernel),
            Some("/proc/4/fd/7/x".into())
        );
        // Outside /proc, a directory may be a link: `/tmp/U/link` may lead
        // anywhere, so its `..` is not `/tmp/U`.
        assert_eq!(
            unresolved(None, "/tmp/U/link/../../../proc/3/stat"),
            pair("/tmp/U/link", "../../../proc/3/stat")
        );
        assert_eq!(
            translate(Path::new("/tmp/U/link/../../../proc/3/stat"), kernel),
            None
        );
        assert_eq!(unresolved(None, "/tmp/link/3"), pair("/tmp/link", "3"));
        // A resolved directory is no link: its `..` is its parent.
        assert_eq!(
            relative("/tmp", "../proc/3/stat"),
            Some("../proc/4/stat".into())
        );
        assert_eq!(unresolved(Some("/tmp"), "../proc/3/stat"), None);
        assert_eq!(
            unresolved(Some("/tmp/U"), "link/../3"),
            pair("/tmp/U/link", "../3")
        );
        // A path that names no number past a directory outside /proc.
        assert_eq!(
            unresolved(None, "/usr/lib/x86_64-linux-gnu/libc.so.6"),
            None
        );
    }

    #[test]
    fn relative_lookups_translate_only_their_own_numbers() {
        // By position, not value: a leader's tid is its pid.
        assert_eq!(
            relative("/proc/3/task", "3/stat"),
            Some(PathBuf::from("4/stat"))
        );
        assert_eq!(relative("/proc/3", "task/3"), Some(PathBuf::from("task/4")));
        let ids =
            |base: &str, rest: &str| Lookup::new(Some(Path::new(base)), Path::new(rest)).ids();
        assert_eq!(ids("/proc/3", "task/3"), [3]);
        assert_eq!(ids("/proc/3", "stat"), Vec::<i32>::new());
        assert_eq!(relative("/proc", "3/stat"), Some(PathBuf::from("4/stat")));
        assert_eq!(relative("/proc/3", "task/5"), Some(PathBuf::from("task/9")));
        assert_eq!(
            relative("/proc/3/task", "5/stat"),
            Some(PathBuf::from("9/stat"))
        );
        assert_eq!(relative("/proc/3", "stat"), None);
        assert_eq!(relative("/proc", "uptime"), None);
        // An absolute path ignores the directory.
        assert_eq!(relative("/proc", "/proc/3"), Some(PathBuf::from("/proc/4")));
    }

    #[test]
    fn ids_are_the_entries_of_proc_and_of_a_task_directory() {
        assert!(names_an_id(Path::new("/proc"), OsStr::new("3")));
        assert!(names_an_id(Path::new("/proc/3/task"), OsStr::new("5")));
        assert!(!names_an_id(Path::new("/proc"), OsStr::new("self")));
        assert!(!names_an_id(Path::new("/proc/3/fd"), OsStr::new("5")));
        assert!(!names_an_id(Path::new("/proc"), OsStr::new("03")));
        // Hermit's own threads keep their numbers.
        assert!(!names_an_id(Path::new("/proc/1/task"), OsStr::new("2")));
    }

    #[test]
    fn the_init_is_where_the_lookup_ends() {
        assert!(names_init(Path::new("/proc/1/task/2/stat")));
        assert!(!names_init(Path::new("/proc/3/stat")));
        assert!(!names_init(Path::new("/proc/self/stat")));
        // Not the first process the path visits.
        assert!(!names_init(Path::new("/proc/1/../3/stat")));
        assert!(names_init(Path::new("/proc/3/../1/task/2/stat")));
        assert_eq!(
            translate(Path::new("/proc/3/../1/task/2/stat"), kernel),
            Some(PathBuf::from("/proc/4/../1/task/2/stat"))
        );
    }
}
