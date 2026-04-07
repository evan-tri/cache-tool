use std::env;
use walkdir::WalkDir;
use blake3::Hasher;
use dirs;
use tar;
use zstd;

fn main() {

    // Collect arguments
    let args: Vec<String> = env::args().collect();

    let output_file = if let Some(pos) = args.iter().position(|arg| arg == "--outputs") {
        &args[pos + 1]
    } else {
        eprintln!("Missing output file");
        std::process::exit(1);
    };
            
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

        //Build blob path
        if let Some(cache_dir) = dirs::cache_dir() {
            let blob_path = cache_dir
                .join("cache-tool")
                .join("blobs")
                .join(&hash_string);

            if std::fs::metadata(&blob_path).is_ok() {
                let file = std::fs::File::open(&blob_path).unwrap();
                let decoder = zstd::Decoder::new(file).unwrap();

                let mut archive = tar::Archive::new(decoder);
                archive.unpack(".").unwrap();

            } else {
                std::fs::create_dir_all(blob_path.parent().unwrap()).unwrap();

                let file = std::fs::File::create(&blob_path).unwrap();
                let encoder = zstd::Encoder::new(file, 3).unwrap();

                let status = std::process::Command::new(&cmd_args[0])
                    .args(&cmd_args[1..])
                    .status()
                    .expect("Failed to run command");

                

                let mut archive = tar::Builder::new(encoder);
                archive.append_path(output_file).unwrap();

                let encoder = archive.into_inner().unwrap();
                encoder.finish().unwrap();

                std::process::exit(status.code().unwrap_or(1));
            }
        }
        
    } else {
        eprintln!("Missing arguments");
        std::process::exit(1);
    }
}

fn should_skip(path: &std::path::Path) -> bool {
    return path.components().any(|x| matches!(x.as_os_str().to_str(), Some(".git") | Some("target") | Some("node_modules")));
}