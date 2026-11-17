//! Parity filesystem fixture (`docs/legacy-parity.md` section 7.1 and 7.2): FX-01..FX-37 and the
//! filesystem variants V2, V5, V6, V7, V9, plus a synthetic large tree generator.
//!
//! Everything lives in one temporary directory on a single filesystem, so hardlinks are real.
//! Run as a non-root user: the permission anomalies must produce `EACCES`.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, utimensat};
use tempfile::TempDir;

/// `n` deterministic bytes for `label`: byte `i` = `(seed(label) + 31 * i) mod 251`.
#[must_use]
pub fn bytes(label: &str, n: usize) -> Vec<u8> {
    let seed = label
        .bytes()
        .fold(7u32, |a, b| a.wrapping_mul(131).wrapping_add(u32::from(b)));
    (0..n)
        .map(|i| u8::try_from((seed as usize + 31 * i) % 251).unwrap_or(0))
        .collect()
}

const COMPLETED: &str = "completed";
const INCOMPLETE: &str = "incomplete";
const MEDIAS: &str = "medias";

const ALPHA: &str = "Alpha.Movie.2020.1080p.WEB.x264-GRP";
const BRAVO: &str = "Bravo.Movie.2021.1080p.BluRay.x265-GRP";
const CHARLIE: &str = "Charlie.Movie.2019.MULTi.1080p.WEB.H264-GRP";
const FOXTROT: &str = "Foxtrot.Show.S01.1080p.WEB.x264-GRP";
const KILO: &str = "Kilo.Movie.2012.1080p.BluRay.x264-GRP";
const NOVEMBER: &str = "November.Movie.2010.1080p.WEB.x264-GRP";
const TANGO: &str = "Tango.Movie.2007.1080p.WEB.x264-GRP";

/// The parity tree. The temporary directory is removed on drop (permissions are restored first).
#[derive(Debug)]
pub struct ParityFixture {
    _tmp: TempDir,
    base: PathBuf,
}

impl ParityFixture {
    /// Builds the base fixture V1.
    pub fn build() -> io::Result<Self> {
        let tmp = tempfile::Builder::new().prefix("removarr-fx-").tempdir()?;
        let base = fs::canonicalize(tmp.path())?;
        let fx = Self { _tmp: tmp, base };
        fx.populate()?;
        Ok(fx)
    }

    #[must_use]
    pub fn base(&self) -> &Path {
        &self.base
    }
    #[must_use]
    pub fn completed(&self) -> PathBuf {
        self.base.join(COMPLETED)
    }
    #[must_use]
    pub fn incomplete(&self) -> PathBuf {
        self.base.join(INCOMPLETE)
    }
    #[must_use]
    pub fn medias(&self) -> PathBuf {
        self.base.join(MEDIAS)
    }
    /// Undeclared sibling: must never be opened or scanned.
    #[must_use]
    pub fn outside(&self) -> PathBuf {
        self.base.join("outside")
    }
    #[must_use]
    pub fn appdata(&self) -> PathBuf {
        self.base.join("appdata")
    }
    /// Path under the fixture, for example `path("completed/stray/readme.txt")`.
    #[must_use]
    pub fn path(&self, rel: &str) -> PathBuf {
        self.base.join(rel)
    }

    /// Uid and gid of the creating user (the `expected_owner` of parity runs).
    pub fn creator_ids(&self) -> io::Result<(u32, u32)> {
        let m = fs::metadata(&self.base)?;
        Ok((m.uid(), m.gid()))
    }

    fn put(&self, rel: &str, label: &str, size: usize) -> io::Result<()> {
        let p = self.base.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(p, bytes(label, size))
    }

    fn link(&self, from: &str, to: &str) -> io::Result<()> {
        let to = self.base.join(to);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::hard_link(self.base.join(from), to)
    }

    fn sym(&self, target: &str, at: &str) -> io::Result<()> {
        let at = self.base.join(at);
        if let Some(parent) = at.parent() {
            fs::create_dir_all(parent)?;
        }
        symlink(target, at)
    }

    fn populate(&self) -> io::Result<()> {
        for d in [COMPLETED, INCOMPLETE, MEDIAS, "outside", "appdata/reports"] {
            fs::create_dir_all(self.base.join(d))?;
        }
        let a = format!("{COMPLETED}/{ALPHA}/{ALPHA}.mkv");
        self.put(&a, "A", 65536)?; // FX-01
        self.put(&format!("{COMPLETED}/{ALPHA}/{ALPHA}.nfo"), "nfoA", 512)?; // FX-02
        self.link(
            &a,
            &format!("{MEDIAS}/movies/Alpha Movie (2020)/Alpha Movie (2020).mkv"),
        )?; // FX-03
        self.put(&format!("{COMPLETED}/{BRAVO}.mkv"), "B", 81920)?; // FX-04
        self.put(
            &format!("{MEDIAS}/movies/Bravo Movie (2021)/{BRAVO}.mkv"),
            "B",
            81920,
        )?; // FX-05
        self.put(&format!("{COMPLETED}/{CHARLIE}/{CHARLIE}.mkv"), "C", 98304)?; // FX-06
        self.put(
            &format!("{COMPLETED}/{CHARLIE}/Sample/charlie.movie.2019.multi.1080p.web.h264-grp.sample.mkv"),
            "Cs",
            8192,
        )?; // FX-07
        self.put(&format!("{COMPLETED}/{CHARLIE}/{CHARLIE}.nfo"), "nfoC", 512)?; // FX-08
        self.put(
            &format!(
                "{MEDIAS}/movies/Charlie Movie (2019)/Charlie Movie (2019) MULTi 1080p WEB.mkv"
            ),
            "C",
            98304,
        )?; // FX-09
        self.put(
            &format!("{COMPLETED}/Delta.Movie.2018.FRENCH.720p.WEB.H264-OLD.mkv"),
            "D1",
            40960,
        )?; // FX-10
        self.put(
            &format!("{MEDIAS}/movies/Delta Movie (2018)/Delta.Movie.2018.MULTi.1080p.BluRay.x265-NEW.mkv"),
            "D2",
            81920,
        )?; // FX-11
        self.put(
            &format!("{COMPLETED}/Echo.Documentary.2017.1080p.WEB.x264-GRP.mkv"),
            "E",
            32768,
        )?; // FX-12
        let f1 = format!("{COMPLETED}/{FOXTROT}/Foxtrot.Show.S01E01.1080p.WEB.x264-GRP.mkv");
        self.put(&f1, "F1", 49152)?; // FX-13
        self.put(
            &format!("{COMPLETED}/{FOXTROT}/Foxtrot.Show.S01E02.1080p.WEB.x264-GRP.mkv"),
            "F2",
            49152,
        )?; // FX-14
        self.link(
            &f1,
            &format!("{MEDIAS}/tv/Foxtrot Show/Season 01/Foxtrot Show - S01E01 - Pilot.mkv"),
        )?; // FX-15
        let g = format!("{COMPLETED}/Golf.Movie.2016.1080p.WEB.x264-GRP.mkv");
        self.put(&g, "G", 16384)?; // FX-16
        self.link(&g, "outside/golf-external-link.mkv")?; // FX-17
        let h = "outside/Hotel.Movie.2015.1080p.WEB.x264-GRP.mkv";
        self.put(h, "H", 16384)?; // FX-18
        self.put("outside/notes.txt", "notes", 64)?; // FX-19
        self.link(
            h,
            &format!("{MEDIAS}/movies/Hotel Movie (2015)/Hotel Movie (2015).mkv"),
        )?; // FX-20
        let india = format!("{COMPLETED}/India.Movie.2014.1080p.WEB.x264-GRP");
        self.put(
            &format!("{india}/India.Movie.2014.1080p.WEB.x264-GRP.mkv"),
            "I",
            16384,
        )?; // FX-21
        self.sym(
            "../../outside/notes.txt",
            &format!("{india}/link-to-outside.txt"),
        )?; // FX-22
        self.sym("../outside", &format!("{COMPLETED}/escape"))?; // FX-23
        let j = format!("{COMPLETED}/Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv");
        self.put(&j, "J", 16384)?; // FX-25
        self.sym(
            "../../../completed/Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv",
            &format!(
                "{MEDIAS}/movies/Juliet Movie (2013)/Juliet.Movie.2013.1080p.WEB.x264-GRP.mkv"
            ),
        )?; // FX-24
        let k = format!("{COMPLETED}/{KILO}/{KILO}.mkv");
        self.put(&k, "K", 65536)?; // FX-26
        self.put(
            &format!("{COMPLETED}/{KILO}/kilo.movie.2012.1080p.bluray.x264-grp-sample.mkv"),
            "Ks",
            8192,
        )?; // FX-27
        self.link(
            &k,
            &format!("{MEDIAS}/movies/Kilo Movie (2012)/Kilo Movie (2012) Bluray-1080p.mkv"),
        )?; // FX-28
        self.put(&format!("{COMPLETED}/stray/readme.txt"), "txt", 128)?; // FX-29
        self.put(&format!("{COMPLETED}/stray/archive.rar"), "rar", 4096)?; // FX-30
        self.put(
            &format!("{COMPLETED}/stray/Unrelated.Film.2010.srt"),
            "srt",
            256,
        )?; // FX-31
        self.put(&format!("{COMPLETED}/{NOVEMBER}.mkv"), "N", 32768)?; // FX-32
        self.put(
            &format!("{MEDIAS}/movies/November Movie (2010)/{NOVEMBER}.mkv"),
            "N",
            32768,
        )?; // FX-33
        let t = format!("{COMPLETED}/{TANGO}.mkv");
        self.put(&t, "T", 24576)?; // FX-34
        self.link(&t, &format!("{COMPLETED}/tango-dupe/{TANGO}.mkv"))?; // FX-35
        self.put(
            &format!("{INCOMPLETE}/Oscar.Movie.2009.1080p.WEB.x264-GRP.mkv"),
            "O",
            8192,
        )?; // FX-36
        self.put(&format!("{INCOMPLETE}/abandoned.partial.mkv"), "ab", 4096)?; // FX-37

        self.stamp_old()?;
        // FX-12 keeps mtime = now; FX-33's parent is made read-only last.
        let now = SystemTime::now();
        set_times(
            &self.base.join(format!(
                "{COMPLETED}/Echo.Documentary.2017.1080p.WEB.x264-GRP.mkv"
            )),
            now,
        )?;
        fs::set_permissions(
            self.base
                .join(format!("{MEDIAS}/movies/November Movie (2010)")),
            fs::Permissions::from_mode(0o555),
        )?;
        Ok(())
    }

    /// mtime = now - 72 h for every entry (symlinks without following), deepest first.
    fn stamp_old(&self) -> io::Result<()> {
        let old = SystemTime::now() - std::time::Duration::from_secs(72 * 3600);
        let mut all = Vec::new();
        collect(&self.base, &mut all)?;
        all.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        for p in all {
            if !p.starts_with(self.base.join("appdata")) {
                set_times(&p, old)?;
            }
        }
        Ok(())
    }

    /// V2: a completed directory that cannot be read (`chmod 0000`). Restored on drop.
    pub fn apply_v2(&self) -> io::Result<PathBuf> {
        let dir = self
            .base
            .join(format!("{COMPLETED}/Lima.Movie.2011.1080p.WEB.x264-GRP"));
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("Lima.Movie.2011.1080p.WEB.x264-GRP.mkv"),
            bytes("L", 16384),
        )?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000))?;
        Ok(dir)
    }

    /// V5: an expected owner that differs from every file (creator uid + 1).
    pub fn apply_v5_expected_owner(&self) -> io::Result<(u32, u32)> {
        let (u, g) = self.creator_ids()?;
        Ok((u + 1, g))
    }

    /// V6: rewrite FX-04 with the same size and different bytes.
    pub fn apply_v6(&self) -> io::Result<()> {
        let p = self.completed().join(format!("{BRAVO}.mkv"));
        let mut data = bytes("B", 81920);
        data[0] ^= 0xFF;
        fs::write(p, data)
    }

    /// V7: flip byte 10000 of FX-09 (outside every 5 x 4096 sample window).
    pub fn apply_v7(&self) -> io::Result<()> {
        let p = self
            .medias()
            .join("movies/Charlie Movie (2019)/Charlie Movie (2019) MULTi 1080p WEB.mkv");
        let mut data = bytes("C", 98304);
        data[10000] ^= 0xFF;
        fs::write(p, data)
    }

    /// V9: qBittorrent working files (`.!qB`, `.parts`) and a download-path-only torrent file.
    pub fn apply_v9(&self) -> io::Result<()> {
        let inc = self.incomplete();
        fs::rename(
            inc.join("Oscar.Movie.2009.1080p.WEB.x264-GRP.mkv"),
            inc.join("Oscar.Movie.2009.1080p.WEB.x264-GRP.mkv.!qB"),
        )?;
        fs::write(
            inc.join(".0123456789abcdef0123456789abcdef01234567.parts"),
            bytes("parts", 1024),
        )?;
        fs::create_dir_all(inc.join("Romeo.Movie"))?;
        fs::write(
            inc.join("Romeo.Movie/Romeo.Movie.mkv.!qB"),
            bytes("R", 4096),
        )
    }

    /// Creates a qBittorrent internal directory (`BT_backup` with a `.fastresume` file).
    pub fn add_qbit_internals(&self, under: &Path) -> io::Result<()> {
        let d = under.join("BT_backup");
        fs::create_dir_all(&d)?;
        fs::write(d.join("0123.fastresume"), b"x")?;
        fs::write(d.join("0123.torrent"), b"x")
    }
}

impl Drop for ParityFixture {
    fn drop(&mut self) {
        // Restore directory permissions so the temp directory can be deleted.
        let mut all = Vec::new();
        let _ = collect_dirs_unreadable_safe(&self.base, &mut all);
        for d in all {
            let _ = fs::set_permissions(&d, fs::Permissions::from_mode(0o755));
        }
    }
}

fn set_times(p: &Path, when: SystemTime) -> io::Result<()> {
    let d = when.duration_since(UNIX_EPOCH).map_err(io::Error::other)?;
    let ts = Timespec {
        tv_sec: i64::try_from(d.as_secs()).map_err(io::Error::other)?,
        tv_nsec: i64::from(d.subsec_nanos()),
    };
    utimensat(
        CWD,
        p,
        &Timestamps {
            last_access: ts,
            last_modification: ts,
        },
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(io::Error::from)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for e in fs::read_dir(dir)? {
        let p = e?.path();
        let md = fs::symlink_metadata(&p)?;
        out.push(p.clone());
        if md.is_dir() {
            collect(&p, out)?;
        }
    }
    out.push(dir.to_path_buf());
    Ok(())
}

/// Collects directories, chmod-ing each to 0755 on the way so unreadable ones can be listed.
fn collect_dirs_unreadable_safe(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    out.push(dir.to_path_buf());
    for e in fs::read_dir(dir)? {
        let p = e?.path();
        if fs::symlink_metadata(&p)?.is_dir() {
            collect_dirs_unreadable_safe(&p, out)?;
        }
    }
    Ok(())
}

/// Shape of a synthetic tree.
#[derive(Debug, Clone, Copy)]
pub struct TreeShape {
    pub depth: u32,
    pub dirs_per_level: u32,
    pub files_per_dir: u32,
    pub file_size: usize,
}

/// Generates a synthetic tree under `root`. Returns `(directories, files)` created below `root`.
pub fn synthetic_tree(root: &Path, shape: TreeShape) -> io::Result<(u64, u64)> {
    fn rec(dir: &Path, level: u32, s: TreeShape, counts: &mut (u64, u64)) -> io::Result<()> {
        let payload = bytes("synthetic", s.file_size);
        for i in 0..s.files_per_dir {
            fs::write(dir.join(format!("file-{i:04}.mkv")), &payload)?;
            counts.1 += 1;
        }
        if level < s.depth {
            for d in 0..s.dirs_per_level {
                let sub = dir.join(format!("dir-{d:03}"));
                fs::create_dir(&sub)?;
                counts.0 += 1;
                rec(&sub, level + 1, s, counts)?;
            }
        }
        Ok(())
    }
    let mut counts = (0, 0);
    rec(root, 0, shape, &mut counts)?;
    Ok(counts)
}
