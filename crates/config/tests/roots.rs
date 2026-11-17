#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::os::unix::fs::symlink;

use common::*;
use removarr_config::{Config, ConfigError, Problem};

fn err_with(f: &Fixture, edit: impl FnOnce(&mut removarr_config::Env)) -> ConfigError {
    let mut e = f.env();
    edit(&mut e);
    Config::from_sources(None, &e).unwrap_err()
}

fn mentions(e: &ConfigError, needle: &str) -> bool {
    e.problems
        .iter()
        .any(|p: &Problem| p.to_string().contains(needle))
}

#[test]
fn missing_roots_refuse_start() {
    let f = Fixture::new();
    let e = err_with(&f, |e| {
        e.remove("REMOVARR_COMPLETED_ROOT");
    });
    assert!(mentions(&e, "roots.completed"));
    let e = err_with(&f, |e| {
        e.remove("REMOVARR_MEDIA_ROOTS");
    });
    assert!(mentions(&e, "roots.media"));
}

#[test]
fn nonexistent_relative_and_traversal_roots() {
    let f = Fixture::new();
    assert!(mentions(
        &err_with(&f, |e| set(e, "REMOVARR_COMPLETED_ROOT", &f.p("nope"))),
        "does not exist"
    ));
    assert!(mentions(
        &err_with(&f, |e| set(e, "REMOVARR_COMPLETED_ROOT", "completed")),
        "absolute"
    ));
    let sneaky = format!("{}/media1/../completed", f.base.display());
    assert!(mentions(
        &err_with(&f, |e| set(e, "REMOVARR_COMPLETED_ROOT", &sneaky)),
        "`..`"
    ));
    assert!(mentions(
        &err_with(&f, |e| set(e, "REMOVARR_COMPLETED_ROOT", "/")),
        "filesystem root"
    ));
    std::fs::write(f.p("file.txt"), "x").unwrap();
    assert!(mentions(
        &err_with(&f, |e| set(e, "REMOVARR_COMPLETED_ROOT", &f.p("file.txt"))),
        "not a directory"
    ));
}

#[test]
fn identical_and_overlapping_roots_refuse_start() {
    let f = Fixture::new();
    // completed == media
    let e = err_with(&f, |e| set(e, "REMOVARR_MEDIA_ROOTS", &f.p("completed")));
    assert!(mentions(&e, "distinct"));
    // duplicate media entries
    let e = err_with(&f, |e| {
        set(
            e,
            "REMOVARR_MEDIA_ROOTS",
            &format!("{0},{0}", f.p("media1")),
        )
    });
    assert!(mentions(&e, "distinct"));
    // media nested inside completed, and completed inside media
    std::fs::create_dir(f.base.join("completed/sub")).unwrap();
    let e = err_with(&f, |e| {
        set(e, "REMOVARR_MEDIA_ROOTS", &f.p("completed/sub"))
    });
    assert!(mentions(&e, "nested"));
    let e = err_with(&f, |e| {
        set(e, "REMOVARR_MEDIA_ROOTS", &f.p("media1"));
        set(e, "REMOVARR_INCOMPLETE_ROOT", &f.p("completed/sub"));
    });
    assert!(mentions(&e, "nested"));
}

#[test]
fn sibling_with_shared_string_prefix_is_not_nested() {
    let f = Fixture::new();
    std::fs::create_dir(f.base.join("completed2")).unwrap();
    let mut e = f.env();
    set(
        &mut e,
        "REMOVARR_MEDIA_ROOTS",
        &format!("{},{}", f.p("media1"), f.p("completed2")),
    );
    assert!(Config::from_sources(None, &e).is_ok());
}

#[test]
fn symlink_root_is_refused() {
    let f = Fixture::new();
    symlink(f.base.join("media2"), f.base.join("link")).unwrap();
    let e = err_with(&f, |e| set(e, "REMOVARR_MEDIA_ROOTS", &f.p("link")));
    assert!(mentions(&e, "symlink"));
    // symlinked parent component
    std::fs::create_dir(f.base.join("media2/inner")).unwrap();
    let e = err_with(&f, |e| set(e, "REMOVARR_MEDIA_ROOTS", &f.p("link/inner")));
    assert!(mentions(&e, "symlink"));
}

#[test]
fn data_dir_must_be_outside_roots_and_contain_none() {
    let f = Fixture::new();
    let e = err_with(&f, |e| set(e, "REMOVARR_DATA_DIR", &f.p("media1")));
    assert!(mentions(&e, "server.data_dir"));
    let e = err_with(&f, |e| {
        set(e, "REMOVARR_DATA_DIR", &f.base.display().to_string())
    });
    assert!(mentions(&e, "server.data_dir"));
    let e = err_with(&f, |e| set(e, "REMOVARR_DATA_DIR", &f.p("nope")));
    assert!(mentions(&e, "server.data_dir"));
}

#[test]
fn commas_and_equals_in_paths_are_rejected() {
    let f = Fixture::new();
    let e = err_with(&f, |e| {
        set(
            e,
            "REMOVARR_QBIT_PATH_MAP",
            &format!("/a=b={}", f.p("completed")),
        )
    });
    assert!(mentions(&e, "path_mappings.qbittorrent"));
    let file = format!("[roots]\ncompleted = \"{}/a=b\"\n", f.base.display());
    let mut env = f.env();
    env.remove("REMOVARR_COMPLETED_ROOT");
    let e = Config::from_sources(Some(&file), &env).unwrap_err();
    assert!(mentions(&e, "roots.completed"));
}
