fn main() {
    let describe = std::process::Command::new("git")
        .args([
            "describe",
            "--always",
            "--dirty",
            "--abbrev=16",
            "--match=''",
        ])
        .output()
        .expect("Couldn't run `git` command");
    assert!(describe.status.success());
    let describe = String::from_utf8_lossy(&describe.stdout);
    println!("cargo:rustc-env=GIT_DESCRIBE={describe}");
}
