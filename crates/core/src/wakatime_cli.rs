//! The person's wakatime-cli, which kairosd runs to see what Hackatime would get from agents' logs, and the official one for
//! people who don't have it yet.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::update::sha256_hex;

/// The wakatime-cli Kairos gets when there is none. The zips' checksums live here instead of being read from the release, so a
/// file changed after Kairos was built is refused.
pub const VERSION: &str = "v2.28.4";
const SUMS: [(&str, &str); 6] = [
    ("darwin-amd64", "e8fed4233155e55a33d06f2bb365abe6f5bf6789b3753dea522ef3dea8bd6884"),
    ("darwin-arm64", "e70db3d824d73939bf4b79146dd3679ff429527b925a10f3d82a7291dec03c6d"),
    ("linux-amd64", "e1dfa9fc4960cb0ebed90ee978c921b8ded9c61ffa63e750ddabd2fedb570345"),
    ("linux-arm64", "77ee19b58880d6c91037584b2f505c3f3fd4e6d550b3b306e72cbfceb3558355"),
    ("windows-amd64", "76f90160dd5268f3b74be2aa75323c9387e79db0874cae71269d1d74c88dd661"),
    ("windows-arm64", "d3d1d6df9383160fa7a95b592890d60bc39098e1dd72344609729dcb0ab18454"),
];
const RELEASES: &str = "https://github.com/wakatime/wakatime-cli/releases/download";
const MAX_ZIP: u64 = 32 << 20;
/// In Kairos's data folder: the wakatime-cli [`install`] put in place, the only one it ever replaces.
const PLACED: &str = "wakatime-cli.json";
/// `kairos install` and kairosd may both find none at once.
const LOCK: &str = "wakatime-cli.lock";

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Placed {
    path: PathBuf,
    version: String,
    /// Of the program, since a plugin that updates it writes over the same file.
    sha256: String,
}

/// wakatime-cli's name for this computer (`GOOS-GOARCH`), for the ones Kairos runs on.
pub fn platform() -> Option<&'static str> {
    platform_of(std::env::consts::OS, std::env::consts::ARCH)
}

fn platform_of(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("linux", "x86_64") => "linux-amd64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "x86_64") => "darwin-amd64",
        ("macos", "aarch64") => "darwin-arm64",
        ("windows", "x86_64") => "windows-amd64",
        ("windows", "aarch64") => "windows-arm64",
        _ => return None,
    })
}

fn file_name(platform: &str) -> String {
    let exe = if platform.starts_with("windows") { ".exe" } else { "" };
    format!("wakatime-cli-{platform}{exe}")
}

/// Where editor plugins and Hackatime's setup keep wakatime-cli: `$WAKATIME_HOME` when set, else `~/.wakatime`, as wakatime-cli
/// itself reads it.
pub fn resources_dir() -> Option<PathBuf> {
    match std::env::var_os("WAKATIME_HOME").filter(|h| !h.is_empty()) {
        Some(home) => Some(PathBuf::from(home)),
        None => Some(dirs::home_dir()?.join(".wakatime")),
    }
}

/// The person's wakatime-cli: the one plugins put in [`resources_dir`], else one on the PATH.
pub fn find() -> Option<PathBuf> {
    find_in(&resources_dir()?, std::env::var_os("PATH").as_deref(), platform()?)
}

fn find_in(dir: &Path, path: Option<&OsStr>, platform: &str) -> Option<PathBuf> {
    let exe = if platform.starts_with("windows") { ".exe" } else { "" };
    let plain = format!("wakatime-cli{exe}");
    [dir.join(file_name(platform)), dir.join(&plain)]
        .into_iter()
        .chain(path.into_iter().flat_map(std::env::split_paths).map(|d| d.join(&plain)))
        .find(|f| f.is_file())
}

/// Gets the official wakatime-cli into [`resources_dir`], where plugins look for it, when the person has none, and puts
/// [`VERSION`] in place of an older one it got before. One Kairos didn't put there is never touched: plugins keep theirs
/// up to date. The one it put in place, if it did.
pub fn install(data: &Path) -> Result<Option<PathBuf>> {
    let platform = platform().context("wakatime-cli has no build for this computer")?;
    let dir = resources_dir().context("cannot find your home folder")?;
    install_with(data, &dir, std::env::var_os("PATH").as_deref(), platform, sum(platform), &|| download(platform))
}

fn install_with(data: &Path, dir: &Path, path: Option<&OsStr>, platform: &str, sum: &str, get: &dyn Fn() -> Result<Vec<u8>>) -> Result<Option<PathBuf>> {
    let name = file_name(platform);
    let dest = dir.join(&name);
    let _lock = lock(data)?;
    #[cfg(windows)]
    let _ = fs::remove_file(aside(&dest));
    let over = match find_in(dir, path, platform) {
        None => false,
        Some(found) => {
            let Some(p) = placed(data).filter(|p| p.path == dest && found == dest && newer(VERSION, &p.version)) else {
                return Ok(None);
            };
            if fs::read(&dest).map(|b| sha256_hex(&b)).ok() != Some(p.sha256) {
                let _ = fs::remove_file(data.join(PLACED));
                return Ok(None);
            }
            true
        }
    };
    let Some(sha256) = place(&get()?, sum, &name, dir, over)? else {
        return Ok(None);
    };
    record(data, &Placed { path: dest.clone(), version: VERSION.to_string(), sha256 })?;
    Ok(Some(dest))
}

/// The official wakatime-cli in `dir`, got unless one is there, with no record of it.
pub fn install_in(dir: &Path) -> Result<PathBuf> {
    let platform = platform().context("wakatime-cli has no build for this computer")?;
    let dest = dir.join(file_name(platform));
    if !dest.is_file() {
        place(&download(platform)?, sum(platform), &file_name(platform), dir, false)?;
    }
    Ok(dest)
}

fn newer(a: &str, b: &str) -> bool {
    let v = |s: &str| semver::Version::parse(s.trim_start_matches('v')).ok();
    matches!((v(a), v(b)), (Some(a), Some(b)) if a > b)
}

fn placed(data: &Path) -> Option<Placed> {
    serde_json::from_str(&fs::read_to_string(data.join(PLACED)).ok()?).ok()
}

fn record(data: &Path, placed: &Placed) -> Result<()> {
    let target = data.join(PLACED);
    let tmp = target.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(placed)?).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &target).with_context(|| format!("writing {}", target.display()))
}

fn lock(data: &Path) -> Result<File> {
    fs::create_dir_all(data).with_context(|| format!("creating {}", data.display()))?;
    let path = data.join(LOCK);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock().with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

fn download(platform: &str) -> Result<Vec<u8>> {
    let url = format!("{RELEASES}/{VERSION}/wakatime-cli-{platform}.zip");
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .user_agent(concat!("kairos/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the http client")?;
    let resp = http.get(&url).send().and_then(|r| r.error_for_status()).with_context(|| format!("downloading {url}"))?;
    let mut zip = Vec::new();
    resp.take(MAX_ZIP + 1).read_to_end(&mut zip).with_context(|| format!("downloading {url}"))?;
    if zip.len() as u64 > MAX_ZIP {
        bail!("{url} is larger than expected");
    }
    Ok(zip)
}

fn sum(platform: &str) -> &'static str {
    SUMS.iter().find(|(p, _)| *p == platform).map_or("", |(_, s)| s)
}

/// Puts the program from the zip in `dir`: over the one there when `over`, else only while there is still none. Its
/// checksum, none when another one turned up first.
fn place(zip: &[u8], sum: &str, name: &str, dir: &Path, over: bool) -> Result<Option<String>> {
    if sha256_hex(zip) != sum {
        bail!("the downloaded wakatime-cli is not the one Kairos expects, so it was not used");
    }
    let bin = unzip_one(zip, name)?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(name);
    let tmp = dir.join(format!("{name}.kairos-tmp"));
    let moved = write_program(&tmp, &bin).and_then(|()| {
        if over {
            swap_in(&tmp, &dest).map(|()| true)
        } else if dest.exists() {
            Ok(false)
        } else {
            fs::rename(&tmp, &dest).map(|()| true)
        }
    });
    if !matches!(moved, Ok(true)) {
        let _ = fs::remove_file(&tmp);
    }
    Ok(moved.with_context(|| format!("writing {}", dest.display()))?.then(|| sha256_hex(&bin)))
}

fn write_program(path: &Path, bin: &[u8]) -> std::io::Result<()> {
    fs::write(path, bin)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn swap_in(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    fs::rename(tmp, dest)
}

/// Windows can't write over a program while it runs (a plugin may be running this one), but it can move it aside, and
/// the next [`install`] deletes it.
#[cfg(windows)]
fn swap_in(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    let old = aside(dest);
    fs::rename(dest, &old)?;
    if let Err(e) = fs::rename(tmp, dest) {
        let _ = fs::rename(&old, dest);
        return Err(e);
    }
    let _ = fs::remove_file(&old);
    Ok(())
}

#[cfg(windows)]
fn aside(dest: &Path) -> PathBuf {
    dest.with_extension("exe.kairos-old")
}

/// The file called `name` in a zip whose checksum was already checked, so only what that zip needs is read: stored or deflated
/// entries, sizes from the central directory (wakatime-cli's zips are written as a stream).
fn unzip_one(zip: &[u8], name: &str) -> Result<Vec<u8>> {
    let u16_at = |i: usize| zip.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize).context("the zip is cut short");
    let u32_at = |i: usize| zip.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).context("the zip is cut short");
    let end = (0..=zip.len().saturating_sub(22)).rev().find(|&i| zip[i..].starts_with(b"PK\x05\x06")).context("not a zip")?;
    let mut at = u32_at(end + 16)? as usize;
    for _ in 0..u16_at(end + 10)? {
        if u32_at(at)? != 0x0201_4b50 {
            bail!("the zip's directory is damaged");
        }
        let (method, crc, packed, size) = (u16_at(at + 10)?, u32_at(at + 16)?, u32_at(at + 20)? as usize, u32_at(at + 24)? as usize);
        let (len, extra, comment, local) = (u16_at(at + 28)?, u16_at(at + 30)?, u16_at(at + 32)?, u32_at(at + 42)? as usize);
        let entry = zip.get(at + 46..at + 46 + len).context("the zip is cut short")?;
        at += 46 + len + extra + comment;
        if entry.rsplit(|&b| b == b'/').next() != Some(name.as_bytes()) {
            continue;
        }
        if u32_at(local)? != 0x0403_4b50 {
            bail!("the zip's {name} entry is damaged");
        }
        let start = local + 30 + u16_at(local + 26)? + u16_at(local + 28)?;
        let data = zip.get(start..start + packed).context("the zip is cut short")?;
        let out = match method {
            0 => data.to_vec(),
            8 => {
                let mut out = Vec::with_capacity(size);
                flate2::read::DeflateDecoder::new(data).take(size as u64 + 1).read_to_end(&mut out).context("unpacking the zip")?;
                out
            }
            m => bail!("the zip uses compression {m}, which Kairos can't unpack"),
        };
        let mut check = flate2::Crc::new();
        check.update(&out);
        if out.len() != size || check.sum() != crc {
            bail!("the zip's {name} entry is damaged");
        }
        return Ok(out);
    }
    bail!("{name} is not in the zip")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            let dir = std::env::temp_dir().join(format!("kairos-wakatime-cli-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Dir(dir)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A zip like Go's archive/zip writes: each entry streamed with its sizes in a data descriptor, the real ones in the directory.
    fn zip_of(files: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let (mut out, mut dir) = (Vec::new(), Vec::new());
        for (name, body, deflate) in files {
            let packed = if *deflate {
                let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                e.write_all(body).unwrap();
                e.finish().unwrap()
            } else {
                body.to_vec()
            };
            let mut crc = flate2::Crc::new();
            crc.update(body);
            let (method, offset) = (if *deflate { 8u16 } else { 0 }, out.len() as u32);
            out.extend(b"PK\x03\x04");
            out.extend(20u16.to_le_bytes());
            out.extend(8u16.to_le_bytes());
            out.extend(method.to_le_bytes());
            out.extend([0u8; 4]);
            out.extend([0u8; 12]);
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name.as_bytes());
            out.extend(&packed);
            out.extend(b"PK\x07\x08");
            out.extend(crc.sum().to_le_bytes());
            out.extend((packed.len() as u32).to_le_bytes());
            out.extend((body.len() as u32).to_le_bytes());
            dir.extend(b"PK\x01\x02");
            dir.extend(20u16.to_le_bytes());
            dir.extend(20u16.to_le_bytes());
            dir.extend(8u16.to_le_bytes());
            dir.extend(method.to_le_bytes());
            dir.extend([0u8; 4]);
            dir.extend(crc.sum().to_le_bytes());
            dir.extend((packed.len() as u32).to_le_bytes());
            dir.extend((body.len() as u32).to_le_bytes());
            dir.extend((name.len() as u16).to_le_bytes());
            dir.extend([0u8; 12]);
            dir.extend(offset.to_le_bytes());
            dir.extend(name.as_bytes());
        }
        let (start, size) = (out.len() as u32, dir.len() as u32);
        out.extend(&dir);
        out.extend(b"PK\x05\x06");
        out.extend([0u8; 4]);
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((files.len() as u16).to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(start.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    #[test]
    fn platforms_are_named_the_way_wakatime_cli_names_its_builds() {
        assert_eq!(platform_of("linux", "x86_64"), Some("linux-amd64"));
        assert_eq!(platform_of("macos", "aarch64"), Some("darwin-arm64"));
        assert_eq!(platform_of("windows", "x86_64"), Some("windows-amd64"));
        assert_eq!(platform_of("freebsd", "x86_64"), None);
        assert_eq!(file_name("windows-arm64"), "wakatime-cli-windows-arm64.exe");
        assert_eq!(file_name("linux-amd64"), "wakatime-cli-linux-amd64");
        for (os, arch) in [("linux", "x86_64"), ("linux", "aarch64"), ("macos", "x86_64"), ("macos", "aarch64"), ("windows", "x86_64"), ("windows", "aarch64")] {
            let s = sum(platform_of(os, arch).unwrap());
            assert!(s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()), "every platform Kairos runs on has a checksum");
        }
    }

    #[test]
    fn the_cli_plugins_installed_comes_before_one_on_the_path() {
        let d = Dir::new("find");
        let (res, bin) = (d.0.join("res"), d.0.join("bin"));
        fs::create_dir_all(&res).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let path = std::env::join_paths([&bin]).unwrap();
        assert_eq!(find_in(&res, Some(&path), "linux-amd64"), None);
        fs::write(bin.join("wakatime-cli"), "").unwrap();
        assert_eq!(find_in(&res, Some(&path), "linux-amd64"), Some(bin.join("wakatime-cli")));
        fs::write(res.join("wakatime-cli"), "").unwrap();
        assert_eq!(find_in(&res, Some(&path), "linux-amd64"), Some(res.join("wakatime-cli")));
        fs::write(res.join("wakatime-cli-linux-amd64"), "").unwrap();
        assert_eq!(find_in(&res, Some(&path), "linux-amd64"), Some(res.join("wakatime-cli-linux-amd64")));
        assert_eq!(find_in(&res, None, "windows-amd64"), None, "windows looks for .exe names");
    }

    #[test]
    fn only_the_expected_zip_is_unpacked() {
        let d = Dir::new("place");
        let program = b"\x7fELF pretend wakatime-cli ".repeat(500);
        let zip = zip_of(&[("README.md", b"hello", false), ("wakatime-cli-linux-amd64", &program, true)]);
        let good = sha256_hex(&zip);

        assert!(place(&zip, &"0".repeat(64), "wakatime-cli-linux-amd64", &d.0, false).is_err());
        assert!(!d.0.join("wakatime-cli-linux-amd64").exists(), "a zip with another checksum is never unpacked");

        let dest = d.0.join("wakatime-cli-linux-amd64");
        assert_eq!(place(&zip, &good, "wakatime-cli-linux-amd64", &d.0, false).unwrap(), Some(sha256_hex(&program)));
        assert_eq!(fs::read(&dest).unwrap(), program);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o755);
        }
        assert_eq!(fs::read_dir(&d.0).unwrap().count(), 1, "no temp file left");
        fs::write(&dest, "one that turned up during the download").unwrap();
        assert_eq!(place(&zip, &good, "wakatime-cli-linux-amd64", &d.0, false).unwrap(), None);
        assert_eq!(fs::read(&dest).unwrap(), b"one that turned up during the download");
        assert_eq!(fs::read_dir(&d.0).unwrap().count(), 1, "no temp file left");

        assert_eq!(unzip_one(&zip, "README.md").unwrap(), b"hello");
        assert!(unzip_one(&zip, "wakatime-cli-darwin-arm64").is_err());
        let mut broken = zip.clone();
        let data = zip.windows(24).position(|w| w == b"wakatime-cli-linux-amd64").unwrap() + 24;
        broken[data + 2] ^= 0xff;
        assert!(unzip_one(&broken, "wakatime-cli-linux-amd64").is_err(), "a damaged entry fails its crc instead of being written");
        assert!(unzip_one(b"not a zip at all, just some bytes", "x").is_err());
    }

    fn release(program: &[u8]) -> (Vec<u8>, String) {
        let zip = zip_of(&[("wakatime-cli-linux-amd64", program, true)]);
        let sum = sha256_hex(&zip);
        (zip, sum)
    }

    /// [`install_with`] on `d`'s data and resources folders. What it did, and how many times it downloaded.
    fn install_from(d: &Dir, zip: &[u8], sum: &str, path: Option<&OsStr>) -> (Result<Option<PathBuf>>, u32) {
        let gets = std::cell::Cell::new(0);
        let get = || -> Result<Vec<u8>> {
            gets.set(gets.get() + 1);
            Ok(zip.to_vec())
        };
        let done = install_with(&d.0.join("data"), &d.0.join("res"), path, "linux-amd64", sum, &get);
        (done, gets.get())
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn with_none_found_the_official_one_is_got_and_recorded() {
        let d = Dir::new("none");
        let (data, dest) = (d.0.join("data"), d.0.join("res").join("wakatime-cli-linux-amd64"));
        let program = b"\x7fELF wakatime-cli ".repeat(100);
        let (zip, sum) = release(&program);

        let (done, gets) = install_from(&d, &zip, &sum, None);
        assert_eq!((done.unwrap(), gets), (Some(dest.clone()), 1));
        assert_eq!(fs::read(&dest).unwrap(), program);
        assert_eq!(files(&d.0.join("res")), ["wakatime-cli-linux-amd64"], "no temp file left");
        assert_eq!(placed(&data), Some(Placed { path: dest.clone(), version: VERSION.into(), sha256: sha256_hex(&program) }));

        let (done, gets) = install_from(&d, &zip, &sum, None);
        assert_eq!((done.unwrap(), gets), (None, 0), "its own, already the version it gets");
    }

    #[test]
    fn a_wakatime_cli_kairos_didnt_put_there_is_never_touched() {
        let d = Dir::new("theirs");
        let (data, res, bin) = (d.0.join("data"), d.0.join("res"), d.0.join("bin"));
        let dest = res.join("wakatime-cli-linux-amd64");
        let (zip, sum) = release(b"the one Kairos gets");

        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("wakatime-cli"), "on the PATH").unwrap();
        let path = std::env::join_paths([&bin]).unwrap();
        let (done, gets) = install_from(&d, &zip, &sum, Some(&path));
        assert_eq!((done.unwrap(), gets), (None, 0));
        assert!(!res.exists());

        fs::create_dir_all(&res).unwrap();
        fs::write(&dest, "a plugin's").unwrap();
        let (done, gets) = install_from(&d, &zip, &sum, None);
        assert_eq!((done.unwrap(), gets), (None, 0));

        // Kairos put an older one here, and a plugin has updated it since.
        record(&data, &Placed { path: dest.clone(), version: "v2.0.0".into(), sha256: sha256_hex(b"Kairos's older one") }).unwrap();
        let (done, gets) = install_from(&d, &zip, &sum, None);
        assert_eq!((done.unwrap(), gets), (None, 0));
        assert_eq!(fs::read(&dest).unwrap(), b"a plugin's");
        assert_eq!(placed(&data), None, "the plugin's from now on");
    }

    #[test]
    fn only_an_older_one_kairos_put_there_is_replaced() {
        let d = Dir::new("older");
        let (data, res) = (d.0.join("data"), d.0.join("res"));
        let dest = res.join("wakatime-cli-linux-amd64");
        let program = b"\x7fELF the pinned wakatime-cli ".repeat(100);
        let (zip, sum) = release(&program);
        fs::create_dir_all(&res).unwrap();
        fs::create_dir_all(&data).unwrap();

        fs::write(&dest, "Kairos's from a later release").unwrap();
        record(&data, &Placed { path: dest.clone(), version: "v99.0.0".into(), sha256: sha256_hex(b"Kairos's from a later release") }).unwrap();
        let (done, gets) = install_from(&d, &zip, &sum, None);
        assert_eq!((done.unwrap(), gets), (None, 0), "never an older one over a newer one");

        fs::write(&dest, "Kairos's older one").unwrap();
        record(&data, &Placed { path: dest.clone(), version: "v2.0.0".into(), sha256: sha256_hex(b"Kairos's older one") }).unwrap();
        let (done, gets) = install_from(&d, &zip, &sum, None);
        assert_eq!((done.unwrap(), gets), (Some(dest.clone()), 1));
        assert_eq!(fs::read(&dest).unwrap(), program);
        assert_eq!(files(&res), ["wakatime-cli-linux-amd64"], "nothing left beside it");
        assert_eq!(placed(&data), Some(Placed { path: dest, version: VERSION.into(), sha256: sha256_hex(&program) }));
    }

    #[test]
    fn a_download_with_another_checksum_leaves_nothing_behind() {
        let d = Dir::new("bad");
        let (data, res) = (d.0.join("data"), d.0.join("res"));
        let dest = res.join("wakatime-cli-linux-amd64");
        let (zip, _) = release(b"changed after Kairos was built");

        let (done, gets) = install_from(&d, &zip, &"0".repeat(64), None);
        assert!(done.is_err());
        assert_eq!(gets, 1);
        assert_eq!(files(&res), Vec::<String>::new());
        assert_eq!(placed(&data), None);

        fs::create_dir_all(&res).unwrap();
        fs::write(&dest, "Kairos's older one").unwrap();
        let older = Placed { path: dest.clone(), version: "v2.0.0".into(), sha256: sha256_hex(b"Kairos's older one") };
        record(&data, &older).unwrap();
        assert!(install_from(&d, &zip, &"0".repeat(64), None).0.is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"Kairos's older one");
        assert_eq!(files(&res), ["wakatime-cli-linux-amd64"]);
        assert_eq!(placed(&data), Some(older), "still Kairos's, to replace next time");
    }

    #[test]
    #[ignore = "downloads the official wakatime-cli for this computer and runs it"]
    fn the_official_wakatime_cli_replaces_an_older_one_kairos_got() {
        let d = Dir::new("official");
        let p = platform().unwrap();
        let (data, res) = (d.0.join("data"), d.0.join("res"));
        let dest = res.join(file_name(p));
        fs::create_dir_all(&res).unwrap();
        fs::create_dir_all(&data).unwrap();
        fs::write(&dest, "Kairos's older one").unwrap();
        record(&data, &Placed { path: dest.clone(), version: "v2.0.0".into(), sha256: sha256_hex(b"Kairos's older one") }).unwrap();

        assert_eq!(install_with(&data, &res, None, p, sum(p), &|| download(p)).unwrap(), Some(dest.clone()));
        let out = std::process::Command::new(&dest).arg("--version").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), VERSION);
        assert_eq!(placed(&data).map(|p| p.version), Some(VERSION.to_string()));
    }

    #[test]
    #[ignore = "downloads every official wakatime-cli zip"]
    fn the_pinned_checksums_match_the_official_zips() {
        let dir = std::env::temp_dir().join(format!("kairos-wakatime-cli-real-{}", std::process::id()));
        for (platform, sum) in SUMS {
            let zip = download(platform).unwrap();
            place(&zip, sum, &file_name(platform), &dir, false).unwrap();
            assert!(fs::metadata(dir.join(file_name(platform))).unwrap().len() > 1 << 20, "{platform}");
        }
        if let Some(p) = platform() {
            let out = std::process::Command::new(dir.join(file_name(p))).arg("--version").output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), VERSION);
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
