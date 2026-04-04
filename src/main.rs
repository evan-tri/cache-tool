use std::env;
use walkdir::WalkDir;
use blake3::Hasher;

fn main() {
    let args: Vec<String> = env::args().collect();

    if let Some(pos) = args.iter().position(|arg| arg == "--") {
        let cmd_args = &args[pos + 1..];
        println!("{:?}", cmd_args);

        let entries: Vec<_> = WalkDir::new(".")
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| !should_skip(e.path()))
        .collect();

        let mut hasher = Hasher::new();
        for entry in &entries {
            let bytes = std::fs::read(entry.path()).unwrap_or_default();
            hasher.update(&bytes);
        }
        let hash_string = hasher.finalize().to_hex().to_string();
        
        let status = std::process::Command::new(&cmd_args[0])
            .args(&cmd_args[1..])
            .status()
            .expect("Failed to run command");
        std::process::exit(status.code().unwrap_or(1));
    } else {
        eprintln!("Missing arguments");
        std::process::exit(1);
    }

    if let Some(cache_dir) = dirs::cache_dir() {
        let blob_path = cache_dir
            .join("cache-tool")
            .join("blobs")
            .join(&hash_string);
    }
}

fn should_skip(path: &std::path::Path) -> bool {
    return path.components().any(|x| matches!(x.as_os_str().to_str(), Some(".git") | Some("target") | Some("node_modules")));
}