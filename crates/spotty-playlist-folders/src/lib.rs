//! Spotify playlist folder storage parser, discovery, upload GC, and tree
//! builder.

pub mod discovery;
pub mod parser;
pub mod tree;

pub use crate::discovery::{
    default_spotify_storage_dir, format_file_size_kb, FolderDiscovery, FolderUploadManager,
    PurgeMode, ASSUMED_HIT_THRESHOLD, MAX_FOLDER_FILE_AGE_SECS,
};
pub use crate::parser::{parse_folder_blob, FolderEntry, MAX_FOLDER_FILE_BYTES};
pub use crate::tree::{build_folder_tree, FolderTreeItem};
