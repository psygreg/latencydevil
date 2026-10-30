//! latencydevil — GPU-aware environment wrapper.
//!
//! Wraps a launch command and injects low-latency environment variables based
//! on the GPUs found on the system, preferring discrete GPUs:
//!
//! * Every GPU:  `PROTON_USE_WAYLAND=1 LOW_LATENCY_LAYER=1 SDL_VIDEO_DRIVER=wayland`
//! * NVIDIA GPU: additionally `LOW_LATENCY_LAYER_REFLEX=1`
//!               and `DXVK_CONFIG="dxgi.hideAmdGpu = True"` (merged into any
//!               existing `DXVK_CONFIG`)
//!
//! Because flatpak sandboxes drop foreign environment variables, `flatpak run
//! <app>` is detected automatically and the variables are forwarded with
//! `--env=KEY=VALUE` flags instead. When `gamemoderun` is found on PATH the
//! command is additionally wrapped with it (GameMode handles the process
//! priorities); without it, the variables alone are injected. Everything else
//! is plain `setenv` + `exec`, so the wrapper never lingers in the process
//! tree.

use std::{
    env, fs,
    io::ErrorKind,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

/// Applied to every wrapped command, regardless of GPU.
const BASE_VARS: &[(&str, &str)] = &[
    ("PROTON_USE_WAYLAND", "1"),
    ("LOW_LATENCY_LAYER", "1"),
    ("SDL_VIDEO_DRIVER", "wayland"),
];

/// Applied on top when an NVIDIA GPU is present.
const NVIDIA_VARS: &[(&str, &str)] = &[
    ("LOW_LATENCY_LAYER_REFLEX", "1"),
    ("DXVK_CONFIG", "dxgi.hideAmdGpu = True"),
];

const SYSFS_PCI: &str = "/sys/bus/pci/devices";
const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
latencydevil {VERSION} — GPU-aware environment wrapper

usage: latencydevil [options] [--] COMMAND [ARGS...]

Wraps COMMAND, injects GPU-aware low-latency environment variables (discrete
GPUs preferred) and replaces itself with COMMAND.

  any GPU:            PROTON_USE_WAYLAND=1 LOW_LATENCY_LAYER=1 SDL_VIDEO_DRIVER=wayland
  NVIDIA GPU present: adds LOW_LATENCY_LAYER_REFLEX=1
                      and DXVK_CONFIG=\"dxgi.hideAmdGpu = True\" (merged into an
                      existing DXVK_CONFIG)
  GameMode:           if `gamemoderun` is on PATH the command runs as
                      `gamemoderun COMMAND` (a command already starting with
                      gamemoderun is kept as-is); without it, only the
                      variables above are injected.

options:
  --print       print the variables that would be set, then exit
  --debug       log GPU detection and applied variables to stderr
  --version     print version and exit
  -h, --help    show this help

examples:
  latencydevil -- %command%                                 (Steam launch options)
  latencydevil -- flatpak run at.anegostudios.VintageStory  (flatpak: injected via --env=...)
";

fn main() -> ExitCode {
    let argv: Vec<std::ffi::OsString> = env::args_os().collect();
    match dispatch(&argv) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("latencydevil: {message}");
            eprint!("{}", help_text());
            ExitCode::from(64)
        }
    }
}

fn help_text() -> String {
    HELP.replace("{VERSION}", VERSION)
}

/// Argument dispatch: leading flags, an optional `--` terminator, then the
/// command to wrap.
fn dispatch(argv: &[std::ffi::OsString]) -> Result<ExitCode, String> {
    let mut print_only = false;
    let mut debug = false;
    let mut index = 1;
    while index < argv.len() {
        match argv[index].to_str() {
            Some("--") => {
                index += 1;
                break;
            }
            Some("--print") => print_only = true,
            Some("--debug") => debug = true,
            Some("--version") => {
                println!("latencydevil {VERSION}");
                return Ok(ExitCode::SUCCESS);
            }
            Some("--help") | Some("-h") => {
                print!("{}", help_text());
                return Ok(ExitCode::SUCCESS);
            }
            // First non-flag argument: the command starts here.
            _ => break,
        }
        index += 1;
    }

    let child_argv = &argv[index..];

    let gpus = detect_gpus();
    let nvidia = gpus.iter().any(|gpu| gpu.vendor == Vendor::Nvidia);
    let vars = build_vars(&gpus);

    let flatpak_detected = flatpak_run_index(child_argv).is_some();
    let gamemode_on_path = find_gamemoderun();
    let child_starts_gamemode = child_argv
        .first()
        .and_then(|arg| base_name(arg))
        .as_deref()
        == Some("gamemoderun");

    if debug {
        if gpus.is_empty() {
            eprintln!("[latencydevil] no GPUs found in {SYSFS_PCI}; applying the universal variables only");
        }
        for gpu in &gpus {
            eprintln!(
                "[latencydevil] GPU {}: {} device {:#06x} class {:#08x} ({})",
                gpu.pci_addr,
                gpu.vendor.name(),
                gpu.device_id,
                gpu.class,
                if gpu.discrete { "discrete" } else { "integrated" },
            );
        }
        eprintln!("[latencydevil] NVIDIA GPU present: {nvidia}");
        for (key, value) in &vars {
            eprintln!("[latencydevil] set {key}={value}");
        }
        if flatpak_detected {
            eprintln!("[latencydevil] flatpak run detected: variables forwarded with --env=");
        }
        eprintln!("[latencydevil] gamemoderun on PATH: {gamemode_on_path}");
        eprintln!(
            "[latencydevil] gamemode wrap: {}",
            gamemode_on_path && !child_starts_gamemode
        );
    }

    if print_only {
        if flatpak_detected {
            println!("# flatpak run detected: variables forwarded with --env=");
        }
        match (gamemode_on_path, child_starts_gamemode) {
            (_, true) => println!("# gamemoderun: command is already wrapped, not double-wrapping"),
            (true, false) => {
                println!("# gamemoderun: found, the command will run as `gamemoderun COMMAND`")
            }
            (false, false) => println!("# gamemoderun: not found on PATH, only injecting variables"),
        }
        for (key, value) in &vars {
            println!("{key}={value}");
        }
        return Ok(ExitCode::SUCCESS);
    }

    if child_argv.is_empty() {
        return Err("no command to run; try `latencydevil --help`".into());
    }

    let final_argv = maybe_wrap_gamemode(
        inject_flatpak_env(child_argv, &vars).unwrap_or_else(|| child_argv.to_vec()),
        gamemode_on_path,
    );
    if debug {
        eprintln!("[latencydevil] exec: {}", format_command(&final_argv));
    }

    for (key, value) in &vars {
        set_env(key, value);
    }

    let (program, args) = final_argv.split_first().expect("checked non-empty");
    let mut command = Command::new(program);
    command.args(args);
    // `exec()` returns an error only when it failed; on success this process
    // is already replaced by the wrapped command.
    let error = command.exec();
    let code = if error.kind() == ErrorKind::NotFound { 127 } else { 126 };
    eprintln!(
        "latencydevil: failed to exec '{}': {error}",
        program.to_string_lossy()
    );
    Ok(ExitCode::from(code))
}

/// Isolate the (edition-dependent) unsafe/safe status of `env::set_var`.
fn set_env(key: &str, value: &str) {
    env::set_var(key, value);
}

fn detect_gpus() -> Vec<Gpu> {
    let mut gpus = Vec::new();
    let Ok(entries) = fs::read_dir(SYSFS_PCI) else {
        return gpus;
    };
    for entry in entries.flatten() {
        let device = entry.path();
        let attributes = (|| {
            let class = read_trim(&device.join("class"))?;
            let vendor = read_trim(&device.join("vendor"))?;
            let product = read_trim(&device.join("device"))?;
            Some((parse_hex(&class)?, parse_hex(&vendor)?, parse_hex(&product)?))
        })();
        let Some((class, vendor_id, device_id)) = attributes else {
            continue;
        };
        // PCI base class 03 = display controller (VGA, 3D or display variant).
        if class >> 16 != 0x03 {
            continue;
        }
        let vendor = Vendor::from_id(vendor_id);
        gpus.push(Gpu {
            pci_addr: entry.file_name().to_string_lossy().into_owned(),
            vendor,
            device_id,
            class,
            discrete: is_discrete(vendor, class),
        });
    }
    // Prefer discrete GPUs: they sort first, stable otherwise.
    gpus.sort_by_key(|gpu| std::cmp::Reverse(gpu.discrete));
    gpus
}

/// Discreteness heuristic. NVIDIA cards are essentially always add-in boards
/// or discrete laptop parts, so they classify as discrete. For every other
/// vendor, iGPUs consistently report the VGA-compatible subclass (0x00) while
/// add-in cards usually report "3D controller" (0x02) or "Display controller"
/// (0x80). GPU handling does not depend on discreteness (NVIDIA presence wins
/// no matter where it sorts); this only orders debug output.
fn is_discrete(vendor: Vendor, class: u32) -> bool {
    match vendor {
        Vendor::Nvidia => true,
        _ => matches!((class >> 8) & 0xff, 0x02 | 0x80),
    }
}

fn build_vars(gpus: &[Gpu]) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = BASE_VARS
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    if gpus.iter().any(|gpu| gpu.vendor == Vendor::Nvidia) {
        for (key, default) in NVIDIA_VARS {
            let value = match *key {
                "DXVK_CONFIG" => merge_dxvk_config(
                    default,
                    env::var_os("DXVK_CONFIG").as_deref().and_then(std::ffi::OsStr::to_str),
                ),
                _ => default.to_string(),
            };
            vars.push((key.to_string(), value));
        }
    }
    vars
}

/// Preserve a user-provided DXVK_CONFIG by appending the wrapped directive,
/// newline-separated (DXVK parses one directive per line).
fn merge_dxvk_config<'a>(extra: &'a str, existing: Option<&str>) -> String {
    match existing.map(str::trim).filter(|current| !current.is_empty()) {
        Some(current) => format!("{current}\n{extra}"),
        None => extra.to_string(),
    }
}

/// `flatpak run` drops foreign environment variables at the sandbox border.
/// Detect that invocation shape and forward the variables with
/// `--env=KEY=VALUE` right after `run` (before any user-supplied options),
/// returning the rewritten argument vector (program first), or `None`.
/// The shape is matched by basename so absolute paths work; a user-provided
/// leading `gamemoderun` is looked through.
fn inject_flatpak_env(
    child_argv: &[std::ffi::OsString],
    vars: &[(String, String)],
) -> Option<Vec<std::ffi::OsString>> {
    let run_index = flatpak_run_index(child_argv)?;
    let mut argv: Vec<std::ffi::OsString> = Vec::with_capacity(vars.len() * 2 + child_argv.len());
    argv.extend_from_slice(&child_argv[..run_index]);
    for (key, value) in vars {
        argv.push(format!("--env={key}={value}").into());
    }
    argv.extend_from_slice(&child_argv[run_index..]);
    Some(argv)
}

/// Index of the element right after `run` for the shapes `flatpak run ...`
/// and `gamemoderun flatpak run ...`, or `None` otherwise.
fn flatpak_run_index(child_argv: &[std::ffi::OsString]) -> Option<usize> {
    match child_argv.len() {
        len if len >= 2
            && arg_base_name(child_argv, 0) == Some("flatpak")
            && arg_base_name(child_argv, 1) == Some("run") =>
        {
            Some(2)
        }
        len if len >= 3
            && arg_base_name(child_argv, 0) == Some("gamemoderun")
            && arg_base_name(child_argv, 1) == Some("flatpak")
            && arg_base_name(child_argv, 2) == Some("run") =>
        {
            Some(3)
        }
        _ => None,
    }
}

/// File name of `argv[index]`, borrowed from the slice so it stays valid in
/// match guards. Non-UTF8 names compare as `None`, which never matches.
fn arg_base_name(argv: &[std::ffi::OsString], index: usize) -> Option<&str> {
    Path::new(argv.get(index)?)
        .file_name()?
        .to_str()
}

/// File name of an argument, tolerating absolute paths.
fn base_name(arg: &std::ffi::OsStr) -> Option<String> {
    Path::new(arg)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// Prepend `gamemoderun` when it is available, unless the command is already
/// wrapped with it (prevents double-wrapping).
fn maybe_wrap_gamemode(
    mut argv: Vec<std::ffi::OsString>,
    gamemode_on_path: bool,
) -> Vec<std::ffi::OsString> {
    let starts_with_gamemoderun =
        argv.first().and_then(|arg| base_name(arg)).as_deref() == Some("gamemoderun");
    if gamemode_on_path && !starts_with_gamemoderun {
        argv.insert(0, "gamemoderun".into());
    }
    argv
}

/// Whether `gamemoderun` is an executable file on PATH.
fn find_gamemoderun() -> bool {
    let dirs: Vec<PathBuf> = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect())
        .unwrap_or_default();
    find_binary_in(&dirs, "gamemoderun")
}

fn find_binary_in(dirs: &[PathBuf], binary: &str) -> bool {
    dirs.iter()
        .any(|dir| is_executable_file(&dir.join(binary)))
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

fn format_command(argv: &[std::ffi::OsString]) -> String {
    argv.iter()
        .map(|arg| {
            let text = arg.to_string_lossy();
            if text.is_empty() || text.contains(char::is_whitespace) {
                format!("'{text}'")
            } else {
                text.into_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn read_trim(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    Some(content.trim().to_string())
}

fn parse_hex(value: &str) -> Option<u32> {
    u32::from_str_radix(value.trim().trim_start_matches("0x"), 16).ok()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Other,
}

impl Vendor {
    fn from_id(id: u32) -> Self {
        match id {
            0x10de => Vendor::Nvidia,
            0x1002 | 0x1022 => Vendor::Amd,
            0x8086 => Vendor::Intel,
            _ => Vendor::Other,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Vendor::Nvidia => "NVIDIA",
            Vendor::Amd => "AMD",
            Vendor::Intel => "Intel",
            Vendor::Other => "other",
        }
    }
}

#[derive(Debug)]
struct Gpu {
    pci_addr: String,
    vendor: Vendor,
    device_id: u32,
    class: u32,
    discrete: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn os_slice(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn test_vars() -> Vec<(String, String)> {
        vec![("SOME_VAR".to_string(), "some value".to_string())]
    }

    #[test]
    fn parses_sysfs_hex() {
        assert_eq!(parse_hex("0x030000"), Some(0x0300_00));
        assert_eq!(parse_hex("0x030000\n"), Some(0x0300_00));
        assert_eq!(parse_hex("10de"), Some(0x10de));
        assert_eq!(parse_hex("garbage"), None);
    }

    #[test]
    fn classifies_vendors() {
        assert_eq!(Vendor::from_id(0x10de), Vendor::Nvidia);
        assert_eq!(Vendor::from_id(0x1002), Vendor::Amd);
        assert_eq!(Vendor::from_id(0x1022), Vendor::Amd);
        assert_eq!(Vendor::from_id(0x8086), Vendor::Intel);
        assert_eq!(Vendor::from_id(0x1234), Vendor::Other);
    }

    #[test]
    fn classifies_discreteness() {
        // NVIDIA: always discrete.
        assert!(is_discrete(Vendor::Nvidia, 0x0300_00));
        assert!(is_discrete(Vendor::Nvidia, 0x0302_00));
        // iGPUs report the VGA-compatible subclass.
        assert!(!is_discrete(Vendor::Amd, 0x0300_00));
        assert!(!is_discrete(Vendor::Intel, 0x0300_00));
        // Non-VGA display subclasses are add-in cards.
        assert!(is_discrete(Vendor::Amd, 0x0302_00));
        assert!(is_discrete(Vendor::Intel, 0x0380_00));
    }

    #[test]
    fn merges_dxvk_config() {
        let dxvk = "dxgi.hideAmdGpu = True";
        assert_eq!(merge_dxvk_config(dxvk, None), dxvk);
        assert_eq!(merge_dxvk_config(dxvk, Some("")), dxvk);
        assert_eq!(merge_dxvk_config(dxvk, Some("   ")), dxvk);
        assert_eq!(
            merge_dxvk_config(dxvk, Some("dxgi.syncInterval = 0")),
            "dxgi.syncInterval = 0\ndxgi.hideAmdGpu = True"
        );
    }

    #[test]
    fn injects_into_flatpak_run() {
        let vars = test_vars();
        let injected = inject_flatpak_env(&os_slice(&["flatpak", "run", "app.id"]), &vars)
            .expect("flatpak run is detected");
        assert_eq!(
            injected,
            os_slice(&["flatpak", "run", "--env=SOME_VAR=some value", "app.id"])
        );

        // User-supplied run options stay behind the injected flags.
        let injected = inject_flatpak_env(&os_slice(&["flatpak", "run", "--user", "app.id"]), &vars)
            .expect("flatpak run is detected");
        assert_eq!(
            injected,
            os_slice(&["flatpak", "run", "--env=SOME_VAR=some value", "--user", "app.id"])
        );
    }

    #[test]
    fn leaves_non_flatpak_commands_alone() {
        let vars = test_vars();
        let cases: Vec<Vec<OsString>> = vec![
            os_slice(&["flatpak", "--version"]),
            os_slice(&["flatpak-spawn", "run", "app.id"]),
            os_slice(&["flatpak"]),
            os_slice(&["/path/to/game.exe"]),
        ];
        for case in cases {
            assert!(inject_flatpak_env(&case, &vars).is_none(), "case: {case:#?}");
        }
    }

    #[test]
    fn injects_into_user_supplied_gamemoderun_flatpak() {
        let vars = test_vars();
        let injected = inject_flatpak_env(
            &os_slice(&["gamemoderun", "flatpak", "run", "app.id"]),
            &vars,
        )
        .expect("gamemoderun + flatpak run is detected");
        assert_eq!(
            injected,
            os_slice(&[
                "gamemoderun",
                "flatpak",
                "run",
                "--env=SOME_VAR=some value",
                "app.id"
            ])
        );
    }

    #[test]
    fn wraps_with_gamemoderun_when_available() {
        let wrapped = maybe_wrap_gamemode(os_slice(&["game"]), true);
        assert_eq!(wrapped, os_slice(&["gamemoderun", "game"]));

        let unwrapped = maybe_wrap_gamemode(os_slice(&["game"]), false);
        assert_eq!(unwrapped, os_slice(&["game"]));

        let already = maybe_wrap_gamemode(os_slice(&["gamemoderun", "game"]), true);
        assert_eq!(already, os_slice(&["gamemoderun", "game"]));

        // Absolute paths to gamemoderun also count as already-wrapped.
        let path_form = maybe_wrap_gamemode(os_slice(&["/usr/bin/gamemoderun", "game"]), true);
        assert_eq!(path_form, os_slice(&["/usr/bin/gamemoderun", "game"]));
    }

    #[test]
    fn detects_executables_on_directories() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "latencydevil-path-test-{0}-{1}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock is sane")
                .as_nanos(),
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        fs::write(dir.join("gamemoderun"), "#!/bin/sh\nexec \"$@\"\n")
            .expect("write test script");
        fs::set_permissions(dir.join("gamemoderun"), fs::Permissions::from_mode(0o755))
            .expect("chmod test script");
        fs::write(dir.join("not-executable"), "data").expect("write test file");

        let dirs = vec![PathBuf::from("/definitely/not/a/dir"), dir.clone()];
        assert!(find_binary_in(&dirs, "gamemoderun"), "executable found");
        assert!(!find_binary_in(&dirs, "not-executable"), "missing exec bit");
        assert!(!find_binary_in(&dirs, "missing"), "absent binary");
        assert!(!find_binary_in(&[], "gamemoderun"), "empty PATH");

        fs::remove_dir_all(&dir).expect("clean up test dir");
    }
}
