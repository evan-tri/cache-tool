use std::env;
use walkdir::WalkDir;
use blake3::Hasher;

fn main() {

    // Collect arguments
    let args: Vec<String> = env::args().collect();

    if let Some(pos) = args.iter().position(|arg| arg == "--") {
        let cmd_args = &args[pos + 1..];

    //Walk directory
        let entries: Vec<_> = WalkDir::new(".")
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| !should_skip(e.path()))
        .collect();

    //Compute hash
        let mut hasher = Hasher::new();
        for entry in &entries {
            let bytes = std::fs::read(entry.path()).unwrap_or_default();
            hasher.update(&bytes);
        }

        let hash_string = hasher.finalize().to_hex().to_string();

        //Check if dir exists in cache
        if let Some(cache_dir) = dirs::cache_dir() {
            let blob_path = cache_dir
                .join("cache-tool")
                .join("blobs")
                .join(&hash_string);

            if std::fs::metadata(&blob_path).is_ok() {

            } else {

            }
            
        }
        
        let status = std::process::Command::new(&cmd_args[0])
            .args(&cmd_args[1..])
            .status()
            .expect("Failed to run command");

        std::process::exit(status.code().unwrap_or(1));

    } else {
        eprintln!("Missing arguments");
        std::process::exit(1);
    }
  
}

fn should_skip(path: &std::path::Path) -> bool {
    return path.components().any(|x| matches!(x.as_os_str().to_str(), Some(".git") | Some("target") | Some("node_modules")));
}