use std::process::Command;

#[test]
fn project_commands_use_the_executable_directory_without_configuration() {
    let root = tempfile::tempdir().unwrap();
    let caller = tempfile::tempdir().unwrap();
    let executable = root.path().join(if cfg!(windows) {
        "tuntex-server.exe"
    } else {
        "tuntex-server"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_tuntex-server"), &executable).unwrap();
    std::fs::write(root.path().join("tuntex-server.yaml"), "invalid YAML: [").unwrap();
    let output = Command::new(&executable)
        .args(["project", "register"])
        .current_dir(caller.path())
        .env("TUNTEX_CONFIG", caller.path().join("missing.yaml"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let key = String::from_utf8(output.stdout).unwrap();
    assert_eq!(key.trim().len(), 64);
    assert!(root.path().join("workspace/projects.sqlite3").is_file());
    assert!(!caller.path().join("workspace").exists());
    let output = Command::new(&executable)
        .args(["project", "list"])
        .current_dir(caller.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains(key.trim()));
    #[cfg(unix)]
    {
        let link = caller.path().join("tuntex-server");
        std::os::unix::fs::symlink(&executable, &link).unwrap();
        let output = Command::new(link)
            .args(["project", "list"])
            .current_dir(caller.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains(key.trim()));
    }
}
