//! Which display mode a launch method can be confirmed to need (#66, spec #59
//! decision 9).
//!
//! Some launch methods bring a console of their own: a `.bat` launcher runs
//! under `cmd.exe`, and a console-subsystem executable gets a console window
//! from Windows. Those are exactly the applications the standalone-window mode
//! exists for — embedding them in the Hub shows the user a Hub terminal with
//! nothing useful in it *and* the application's own console, which is the
//! duplication spec #59's problem statement describes.
//!
//! ## What this is allowed to decide, and what it is not
//!
//! The recommendation is read off the **launch method** — the command's
//! executable and the subsystem Windows records inside it — and never off the
//! entry's name. That is the difference decision 9 draws: two entries called
//! ComfyUI, one starting `run_nvidia_gpu.bat` and one starting a launcher
//! `.exe`, are different launch methods and get different answers, because the
//! Hub looked at what would actually run rather than at the word "ComfyUI".
//!
//! Everything this cannot confirm stays **unrecommended** rather than guessed
//! at: a program that cannot be resolved, a GUI executable (which may or may
//! not open a console of its own — a launcher does, a window does not), a file
//! whose header is not a PE image. Both display modes stay offered for those,
//! and the reason says so (decision 9: "未知应用保留可选择的模式，不声称已完成
//! 自动识别").

use std::path::{Path, PathBuf};

use crate::config::DisplayMode;
use crate::session::core::split_command;

/// What the Hub can say about one launch method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayAdvice {
    /// The mode to start the form on, when this build can confirm one.
    pub recommended: Option<DisplayMode>,
    /// Why — a sentence for the form, whether or not there is a recommendation.
    pub reason: String,
    /// The executable the command's first word resolved to, when it resolved.
    ///
    /// Reported so the user can see what the Hub actually looked at; a
    /// recommendation nobody can check would be the "自动识别" claim decision 9
    /// forbids.
    pub program: Option<String>,
}

/// What kind of program the launch method starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchKind {
    /// Windows gives it a console: a batch file (hosted by `cmd.exe`) or a
    /// console-subsystem image.
    Console,
    /// A GUI-subsystem image: it has no console, and whether it opens one of
    /// its own is not something a header can answer.
    Graphical,
}

/// Advise a display mode for `command`, resolved against `cwd` and `PATH`.
pub fn advise(command: &str, cwd: Option<&str>) -> DisplayAdvice {
    if command.trim().is_empty() {
        return DisplayAdvice {
            recommended: None,
            reason: "还没有填写启动命令；填写后这里会说明 Hub 能否确认它自带控制台。".to_owned(),
            program: None,
        };
    }

    // Split the way a *start* splits it (`session::core::split_command`), so
    // the advice is about the program that would actually run rather than about
    // what a second tokenizer here believed it read.
    let token = match split_command(command) {
        Ok((program, _)) => program.to_string_lossy().into_owned(),
        Err(reason) => {
            return DisplayAdvice {
                recommended: None,
                reason: format!(
                    "还无法确认这条命令的启动方式（{reason}）；两种显示方式都可以，请按需要选择。"
                ),
                program: None,
            }
        }
    };

    let Some(program) = resolve_program(&token, cwd) else {
        return DisplayAdvice {
            recommended: None,
            reason: format!(
                "找不到 `{token}`，无法确认它的启动方式；两种显示方式都可以，请按需要选择。"
            ),
            program: None,
        };
    };
    let shown = program.to_string_lossy().into_owned();

    match launch_kind(&program) {
        Some(LaunchKind::Console) => DisplayAdvice {
            recommended: Some(DisplayMode::Window),
            reason: format!(
                "`{shown}` 自带控制台（{detail}），独立窗口模式下它会显示自己的窗口和控制台，\
                 Hub 不会重复内嵌。",
                detail = if is_batch(&program) {
                    "批处理由 cmd.exe 托管"
                } else {
                    "控制台子系统程序"
                }
            ),
            program: Some(shown),
        },
        Some(LaunchKind::Graphical) => DisplayAdvice {
            recommended: None,
            reason: format!(
                "`{shown}` 是图形界面程序：Hub 无法只凭启动方式确认它是否自带控制台\
                 （启动器会开一个，普通窗口程序不会）；两种显示方式都可以，请按需要选择。"
            ),
            program: Some(shown),
        },
        None => DisplayAdvice {
            recommended: None,
            reason: format!(
                "`{shown}` 不是可识别的可执行文件，Hub 无法确认它是否自带控制台；\
                 两种显示方式都可以，请按需要选择。"
            ),
            program: Some(shown),
        },
    }
}

/// Where the command's program is, as the OS would find it.
///
/// A name with a separator in it is a path, relative to the configured working
/// directory when it is not absolute. A bare name is searched for in the
/// working directory first (which is what the user sees when they test the
/// command by hand) and then along `PATH`, with the extensions Windows would
/// try.
///
/// Shared with the association search (#67) rather than copied: "which file
/// would this configuration start" has one answer, and a second resolution that
/// disagreed with this one by a directory or an extension would make the Hub
/// look for a different program than the one it launches.
pub(crate) fn resolve_program(token: &str, cwd: Option<&str>) -> Option<PathBuf> {
    let looks_like_path = token.contains('/') || token.contains('\\');
    let cwd = cwd.map(PathBuf::from);

    if looks_like_path {
        let path = Path::new(token);
        let candidate = match (path.is_absolute(), &cwd) {
            (true, _) => path.to_path_buf(),
            (false, Some(cwd)) => cwd.join(path),
            (false, None) => path.to_path_buf(),
        };
        return existing(&candidate);
    }

    if let Some(cwd) = &cwd {
        if let Some(found) = existing(&cwd.join(token)) {
            return Some(found);
        }
    }
    for directory in std::env::split_paths(&std::env::var_os("PATH")?) {
        if let Some(found) = existing(&directory.join(token)) {
            return Some(found);
        }
    }
    None
}

/// The file itself, or the same name with a Windows executable extension.
fn existing(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if path.extension().is_none() {
        for extension in ["exe", "cmd", "bat", "com"] {
            let candidate = path.with_extension(extension);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Whether this file is a batch script — read by `cmd.exe` rather than loaded
/// as an image.
///
/// Shared with the association search (#67) rather than copied: "is this a
/// script" decides both what the advice says about the launch method and which
/// process the Hub looks for, and two answers to it would be two chances to
/// disagree about the same file.
pub(crate) fn is_batch(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("bat") | Some("cmd")
    )
}

/// Which kind of program `path` starts, or `None` when this build cannot tell.
fn launch_kind(path: &Path) -> Option<LaunchKind> {
    // A batch file is not a PE image at all: `cmd.exe` reads it line by line,
    // and the console the user sees is that shell's.
    if is_batch(path) {
        return Some(LaunchKind::Console);
    }
    match pe_subsystem(path)? {
        // IMAGE_SUBSYSTEM_WINDOWS_CUI — the console subsystem.
        3 => Some(LaunchKind::Console),
        // IMAGE_SUBSYSTEM_WINDOWS_GUI — no console of its own.
        2 => Some(LaunchKind::Graphical),
        _ => None,
    }
}

/// The `Subsystem` field of a PE image's optional header.
///
/// A deliberately small reader for one field: the file is opened, two headers
/// are checked, and one `u16` is read. Anything that does not look like the
/// image this expects — a directory, a shortcut, a POSIX script, a truncated
/// download — is reported as "cannot tell" rather than guessed at, which is what
/// keeps this function from being an identification engine (decision 9).
fn pe_subsystem(path: &Path) -> Option<u16> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 64];
    file.read_exact(&mut header).ok()?;
    if &header[0..2] != b"MZ" {
        return None;
    }
    let pe_offset = u32::from_le_bytes(header[0x3c..0x40].try_into().ok()?) as u64;

    let mut signature = [0u8; 4];
    file.seek(SeekFrom::Start(pe_offset)).ok()?;
    file.read_exact(&mut signature).ok()?;
    if &signature != b"PE\0\0" {
        return None;
    }

    // The optional header begins right after the 20-byte COFF header. The
    // `Subsystem` sits at the same offset in the 32- and 64-bit layouts: PE32
    // carries a 4-byte `BaseOfData` the 64-bit layout drops, and the 64-bit
    // layout widens `ImageBase` to 8 bytes — the two differences cancel, so
    // one offset serves both. At PE32's `CheckSum` (offset 64) the field this
    // read used to land on, a 32-bit image's console/GUI answer was whatever
    // its checksum's low word happened to be.
    let optional = pe_offset + 4 + 20;
    let mut magic = [0u8; 2];
    file.seek(SeekFrom::Start(optional)).ok()?;
    file.read_exact(&mut magic).ok()?;
    if !matches!(u16::from_le_bytes(magic), 0x10b | 0x20b) {
        return None;
    }

    let mut subsystem = [0u8; 2];
    file.seek(SeekFrom::Start(optional + 68)).ok()?;
    file.read_exact(&mut subsystem).ok()?;
    Some(u16::from_le_bytes(subsystem))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// The core of the rule: what the *launch method* is decides, and two
    /// entries that would both be called "ComfyUI" are not the same launch
    /// method (spec #59 decision 9).
    #[test]
    fn the_same_name_with_two_launch_methods_is_advised_differently() {
        let fixture = BatchFixture::new("comfy");

        let batched = advise(&fixture.command(), None);
        let graphical = advise("explorer.exe", None);

        assert_eq!(
            batched.recommended,
            Some(DisplayMode::Window),
            "{batched:?}"
        );
        assert_eq!(
            graphical.recommended, None,
            "a GUI program's console cannot be confirmed: {graphical:?}"
        );
        assert!(batched.reason.contains("cmd.exe"), "{batched:?}");
    }

    /// A console-subsystem executable is confirmed by its own header, not by
    /// its name.
    #[test]
    fn a_console_subsystem_program_is_recommended_the_standalone_window() {
        let advice = advise("cmd.exe", None);

        assert_eq!(advice.recommended, Some(DisplayMode::Window), "{advice:?}");
        assert!(advice.program.is_some(), "the file it looked at is named");
        assert!(
            advice.reason.contains("控制台子系统"),
            "the reason says what was confirmed: {advice:?}"
        );
    }

    /// A GUI-subsystem program is *not* guessed at: whether it opens a console
    /// of its own is not in its header, and decision 9 keeps the Hub from
    /// claiming a recognition it did not do.
    #[test]
    fn a_graphical_program_keeps_both_modes_available() {
        let advice = advise("explorer.exe", None);

        assert_eq!(advice.recommended, None, "{advice:?}");
        assert!(advice.reason.contains("两种显示方式"), "{advice:?}");
    }

    /// A command the session layer could not split is one it could not start,
    /// so there is nothing to be confirmed about its display either — and the
    /// advice says which of the two it is.
    #[test]
    fn a_command_that_cannot_be_split_advises_nothing() {
        let advice = advise("\"unclosed start.cmd", None);

        assert_eq!(advice.recommended, None, "{advice:?}");
        assert_eq!(advice.program, None);
        assert!(advice.reason.contains("还无法确认"), "{advice:?}");
    }

    /// The program is resolved along `PATH` for a bare name, not only as a path
    /// relative to the working directory — which is how a user writes
    /// `python main.py` or `cmd.exe /c run.bat`.
    #[test]
    fn a_bare_name_is_resolved_on_the_path() {
        let advice = advise("cmd.exe /c run.bat", None);

        assert_eq!(advice.recommended, Some(DisplayMode::Window), "{advice:?}");
        let program = advice.program.expect("a resolved program is reported");
        assert!(
            program.to_ascii_lowercase().ends_with("cmd.exe"),
            "the resolved file is named: {program}"
        );
    }

    /// A command whose program cannot be found advises nothing rather than
    /// guessing from the rest of the string.
    #[test]
    fn an_unresolvable_program_advises_nothing() {
        let advice = advise("definitely-not-a-real-program-66 --serve", None);

        assert_eq!(advice.recommended, None, "{advice:?}");
        assert_eq!(advice.program, None);
        assert!(advice.reason.contains("找不到"), "{advice:?}");
    }

    /// A command that has not been filled in yet is a state the form is in, not
    /// an error.
    #[test]
    fn an_empty_command_advises_nothing() {
        let advice = advise("   ", None);

        assert_eq!(advice.recommended, None);
        assert_eq!(advice.program, None);
        assert!(advice.reason.contains("还没有填写"), "{advice:?}");
    }

    /// A path is resolved against the configured working directory, and a
    /// quoted path with spaces in it is one token rather than three.
    #[test]
    fn a_quoted_path_resolves_against_the_working_directory() {
        let fixture = BatchFixture::new("with space");

        let advice = advise(&fixture.command(), Some(&fixture.cwd()));

        assert_eq!(advice.recommended, Some(DisplayMode::Window), "{advice:?}");
        assert_eq!(
            advice.program.as_deref(),
            Some(fixture.path().to_string_lossy().as_ref())
        );
    }

    /// A directory is not a program, and neither is a file that only pretends
    /// to be one: both stay unresolvable to a kind.
    #[test]
    fn something_that_is_not_an_image_is_not_recognised() {
        let directory = std::env::temp_dir();
        assert!(launch_kind(&directory).is_none());

        let not_an_image = directory.join("lch-not-an-image.exe");
        std::fs::write(&not_an_image, b"#!/bin/sh\necho hello\n").expect("the fixture is written");
        assert!(launch_kind(&not_an_image).is_none());
        let _ = std::fs::remove_file(&not_an_image);
    }

    /// The `Subsystem` field sits at the same optional-header offset in the
    /// 32- and 64-bit layouts — their differences (`BaseOfData`, the wider
    /// `ImageBase`) cancel — and not at `CheckSum`, whose low word a 32-bit
    /// image's subsystem answer used to be read from.
    #[test]
    fn a_pe32_image_reports_its_own_subsystem_field() {
        let mut image = vec![0u8; 160];
        image[0..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        image[0x40..0x44].copy_from_slice(b"PE\0\0");
        // The optional header starts at 0x58. The checksum's low word holds
        // the console value, so an offset-64 read would call this image a
        // console program whatever its real answer was.
        let optional = 0x58usize;
        let checksum_low_word = 3u16;
        let subsystem = 2u16; // IMAGE_SUBSYSTEM_WINDOWS_GUI

        image[optional..optional + 2].copy_from_slice(&0x10bu16.to_le_bytes()); // PE32
        image[optional + 64..optional + 66].copy_from_slice(&checksum_low_word.to_le_bytes());
        image[optional + 68..optional + 70].copy_from_slice(&subsystem.to_le_bytes());

        let path = std::env::temp_dir().join("lch-pe-subsystem-fixture.exe");
        std::fs::write(&path, &image).expect("the PE32 fixture is written");
        assert_eq!(launch_kind(&path), Some(LaunchKind::Graphical));

        // The 64-bit layout answers the same field at the same offset.
        image[optional..optional + 2].copy_from_slice(&0x20bu16.to_le_bytes()); // PE32+
        image[optional + 68..optional + 70].copy_from_slice(&3u16.to_le_bytes());
        std::fs::write(&path, &image).expect("the PE32+ fixture is written");
        assert_eq!(launch_kind(&path), Some(LaunchKind::Console));
        let _ = std::fs::remove_file(&path);
    }

    /// A batch file in a scratch directory, with a path a user could actually
    /// configure.
    struct BatchFixture(PathBuf);

    impl BatchFixture {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "lch-recommend-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("the clock is after the epoch")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).expect("the fixture directory is created");
            std::fs::write(path.join("start.cmd"), "@echo off\r\necho starting\r\n")
                .expect("the fixture script is written");
            BatchFixture(path)
        }

        fn path(&self) -> PathBuf {
            self.0.join("start.cmd")
        }

        fn cwd(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }

        /// The command as a user writes it: the program, quoted, then nothing
        /// else.
        fn command(&self) -> String {
            format!("\"{}\"", self.path().display())
        }
    }

    impl Drop for BatchFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
