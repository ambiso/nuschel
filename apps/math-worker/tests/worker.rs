// SPDX-License-Identifier: EUPL-1.2
//! The real math worker in its real sandbox, as the desktop drives it.
#![cfg(target_os = "linux")]
use std::{path::PathBuf, time::Instant};

use nuschel_ipc::{
    host::Error,
    math::{MathRequest, MathWorker},
};

fn worker() -> MathWorker {
    MathWorker::new(env!("CARGO_BIN_EXE_nuschel-math-worker").into())
}

fn request(tex: &str, display: bool) -> MathRequest {
    MathRequest {
        tex: tex.into(),
        display,
        size: 20.,
    }
}

#[tokio::test]
async fn formulas_render_in_one_long_lived_sandboxed_worker() {
    let mut worker = worker();
    assert!(
        !worker.running(),
        "started on the first formula, not before"
    );
    let inline = worker.render(&request("x^2", false)).await.unwrap();
    let coverage = inline.decode().expect("a consistent image");
    assert!(coverage.iter().any(|&alpha| alpha > 128));
    assert!(inline.baseline > 0 && inline.baseline < inline.height);
    assert!(worker.running());
    let display = worker
        .render(&request("\\sum_{i=1}^n i", true))
        .await
        .unwrap();
    assert!(display.height > inline.height);
    // A formula that does not render is an answer; the worker stays up.
    match worker.render(&request("\\frac{a}", false)).await {
        Err(Error::Rejected(reason)) => assert_eq!(reason, "an argument is missing"),
        other => panic!("{other:?}"),
    }
    assert!(worker.running());
    worker.stop().await;
    assert!(!worker.running());
    // The next formula starts it again.
    worker.render(&request("y", false)).await.unwrap();
    worker.stop().await;
}

#[tokio::test]
async fn hostile_formulas_are_refused_or_rendered_within_their_limits() {
    let mut worker = worker();
    let deep = "{".repeat(200) + &"}".repeat(200);
    let wide = vec!["\\frac{1}{2}"; 400].join("+");
    let row = vec!["x"; 20].join("&");
    let matrix = format!(
        "\\begin{{pmatrix}}{}\\end{{pmatrix}}",
        vec![row; 20].join("\\\\")
    );
    let nested_fractions = "\\frac{".repeat(30) + "x" + &"}{y}".repeat(30);
    for tex in [
        "#read(\"/etc/passwd\")".to_string(),
        "\\text{\" #read(\"/etc/hostname\") \"}".into(),
        "$ #import \"@preview/mitex:0.2.4\": * $".into(),
        "\\iftypst #read(\"/etc/passwd\") \\fi".into(),
        deep,
        wide,
        matrix,
        nested_fractions,
        "x".repeat(5000),
    ] {
        let started = Instant::now();
        match worker.render(&request(&tex, true)).await {
            Ok(image) => assert!(image.decode().is_some()),
            Err(Error::Rejected(_)) => {}
            Err(error) => panic!("{tex:.40}: {error}"),
        }
        assert!(started.elapsed().as_secs() < 2, "{tex:.40}");
        assert!(worker.running(), "{tex:.40}");
    }
    // What `#read` would have read never appears: the text is drawn as text.
    let shown = worker
        .render(&request("\\text{#read(\"/etc/hostname\")}", false))
        .await
        .unwrap();
    assert!(shown.width > 100);
    worker.stop().await;
}

/// A worker that never answers is killed at its deadline, and the next
/// formula gets a fresh one.
#[tokio::test]
async fn a_worker_that_misses_its_deadline_is_killed() {
    // A stand-in that reads and never answers; it runs in the same sandbox,
    // so its interpreter must be in the Nix store (bound read-only).
    let Some(bash) = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join("bash"))
        .find_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with("/nix/store"))
    else {
        eprintln!("skipped: no bash in the Nix store");
        return;
    };
    let directory = std::env::temp_dir().join(format!("nuschel-math-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let silent: PathBuf = directory.join("silent-worker");
    std::fs::write(
        &silent,
        format!(
            "#!{}\nwhile IFS= read -r -d '' -n 1 _; do :; done\n",
            bash.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&silent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut worker = MathWorker::new(silent);
    let started = Instant::now();
    let result = worker.render(&request("x", false)).await;
    assert!(matches!(result, Err(Error::Timeout)), "{result:?}");
    assert!((3..6).contains(&started.elapsed().as_secs()));
    assert!(!worker.running(), "killed and forgotten");
    std::fs::remove_dir_all(&directory).ok();
}

/// `cargo test --release -p nuschel-math-worker --test worker -- --ignored --nocapture`
#[tokio::test]
#[ignore = "measurement, not a check"]
async fn cold_start_and_formula_cost() {
    for _ in 0..3 {
        let mut worker = worker();
        let started = Instant::now();
        worker.render(&request("x", false)).await.unwrap();
        let cold = started.elapsed();
        let started = Instant::now();
        for index in 0..50 {
            let tex = format!("\\sum_{{i=1}}^{{{index}}} \\frac{{i^2}}{{\\sqrt{{i+1}}}}");
            worker.render(&request(&tex, true)).await.unwrap();
        }
        println!(
            "cold start with a first formula: {:.1} ms; then {:.2} ms per display formula",
            cold.as_secs_f64() * 1e3,
            started.elapsed().as_secs_f64() * 1e3 / 50.
        );
        worker.stop().await;
    }
}
