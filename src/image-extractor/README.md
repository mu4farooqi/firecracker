# Image Extractor

A Rust binary for extracting Docker image layers into deterministic file systems, following AWS Lambda's approach for block-level deduplication.

## Features

- **Deterministic Extraction**: Creates reproducible file systems from Docker images
- **Layer Separation**: Separates base image layers from additional layers
- **Block-level Chunking**: Creates fixed-size chunks (512KB by default) for deduplication
- **Content-addressable Storage**: Chunks are named by their content hash
- **Entrypoint Extraction**: Generates executable shell scripts from Docker ENTRYPOINT/CMD

## Usage

### Basic Usage

```bash
# Extract a Docker image
./image-extractor myapp:latest --base-image rowerdev/dev-base

# Specify custom output directory
./image-extractor myapp:latest --base-image rowerdev/dev-base --output-dir /path/to/output

# Use custom chunk size (in KB)
./image-extractor myapp:latest --base-image rowerdev/dev-base --chunk-size 1024
```

### Build

```bash
# Build using the devtool
tools/devtool build --release

# The binary will be available at:
# build/cargo_target/{arch}-unknown-linux-musl/release/image-extractor
```

## Output Structure

The tool creates the following output structure:

```
output_dir/
├── base_fs/                 # Base image filesystem
├── additional_fs/           # Additional layers filesystem
├── entrypoint.sh           # Executable entrypoint script
├── base_chunks/            # Content-addressable chunks for base_fs
│   ├── block_mapping.json  # Mapping for reconstructing base_fs
│   └── *.chunk            # Individual chunk files
└── additional_chunks/      # Content-addressable chunks for additional_fs
    ├── block_mapping.json  # Mapping for reconstructing additional_fs
    └── *.chunk            # Individual chunk files
```

## Algorithm

### Deterministic Flattening

The tool implements deterministic flattening similar to AWS Lambda:

1. **Consistent Timestamps**: All files use a fixed timestamp (2022-01-01 00:00:00 UTC)
2. **Sorted Processing**: Files are processed in lexicographic order
3. **Deterministic Permissions**: File permissions are preserved but normalized
4. **Serial Operations**: All operations are performed serially to avoid non-determinism

### Layer Identification

The tool uses heuristics to identify base layers vs additional layers:

- System paths (`/bin/`, `/lib/`, `/usr/bin/`, etc.) → Base layers
- Application-specific paths → Additional layers
- Configurable base image pattern matching

### Block-level Chunking

- Files are split into fixed-size chunks (512KB default)
- Each chunk is padded to the full chunk size
- Chunks are named using SHA256 hash of their content
- Identical chunks across different images share the same storage
- Block mapping JSON files enable reconstruction

## Dependencies

- Docker daemon must be running and accessible
- Sufficient disk space for temporary containers and extracted files
- Unix-like system (for file permissions and symlinks)

## Limitations

- Requires Docker daemon access
- Currently uses heuristics for layer identification (could be improved)
- File ownership changes require root privileges (currently skipped)
- Large images may require significant temporary disk space

## Future Improvements

- More sophisticated base layer detection using image history
- Support for OCI image format
- Parallel chunk processing
- Compression of chunk files
- Integration with container registries
