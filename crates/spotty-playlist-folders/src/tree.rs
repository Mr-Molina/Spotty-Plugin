//! Hierarchical playlist folder tree builder with empty-folder pruning.

use std::collections::HashMap;
use std::collections::HashSet;

use crate::discovery::normalize_playlist_uri;
use crate::parser::FolderEntry;

const ROOT_PARENT: &str = "/";

/// A node in the rendered playlist folder hierarchy tree.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderTreeItem<T> {
    /// A non-empty folder containing ordered child folders and leaves.
    Folder {
        /// Folder identifier from the Spotify storage blob.
        id: String,
        /// Decoded folder display name.
        name: String,
        /// Zero-based sort order within the parent container.
        order: usize,
        /// Recursively pruned and ordered child items.
        children: Vec<FolderTreeItem<T>>,
    },
    /// A leaf playlist item.
    Leaf {
        /// Sort order from the hierarchy, or `usize::MAX` if unmapped.
        order: usize,
        /// Cloned playlist item supplied by the caller.
        item: T,
    },
}

impl<T> FolderTreeItem<T> {
    /// Returns the sort order key for this node.
    #[must_use]
    pub fn order(&self) -> usize {
        match self {
            Self::Folder { order, .. } | Self::Leaf { order, .. } => *order,
        }
    }
}

struct FolderMeta<'a> {
    id: &'a str,
    name: &'a str,
    order: usize,
}

/// Groups `playlists` according to `hierarchy`, sorts each level by `order`,
/// recursively prunes empty folders, and appends unmapped playlists at root.
#[must_use]
pub fn build_folder_tree<T, F>(
    playlists: &[T],
    hierarchy: &HashMap<String, FolderEntry>,
    uri_of: F,
) -> Vec<FolderTreeItem<T>>
where
    T: Clone,
    F: Fn(&T) -> &str,
{
    let mut known_folders: HashSet<&str> = HashSet::new();
    let mut folders_by_parent: HashMap<&str, Vec<FolderMeta<'_>>> = HashMap::new();

    for entry in hierarchy.values() {
        let FolderEntry::Folder {
            id,
            name,
            parent,
            order,
        } = entry
        else {
            continue;
        };
        known_folders.insert(id.as_str());
        let parent_key = if parent.is_empty() {
            ROOT_PARENT
        } else {
            parent.as_str()
        };
        folders_by_parent
            .entry(parent_key)
            .or_default()
            .push(FolderMeta {
                id: id.as_str(),
                name: name.as_str(),
                order: *order,
            });
    }

    let mut leaves_by_parent: HashMap<&str, Vec<FolderTreeItem<T>>> = HashMap::new();
    let mut unmapped_root: Vec<FolderTreeItem<T>> = Vec::new();

    for playlist in playlists {
        let raw_uri = uri_of(playlist);
        let norm_uri = normalize_playlist_uri(raw_uri);
        match hierarchy.get(norm_uri.as_ref()) {
            Some(FolderEntry::Playlist { parent, order, .. }) => {
                let target_parent =
                    if parent == ROOT_PARENT || known_folders.contains(parent.as_str()) {
                        parent.as_str()
                    } else {
                        ROOT_PARENT
                    };
                leaves_by_parent
                    .entry(target_parent)
                    .or_default()
                    .push(FolderTreeItem::Leaf {
                        order: *order,
                        item: playlist.clone(),
                    });
            }
            _ => {
                unmapped_root.push(FolderTreeItem::Leaf {
                    order: usize::MAX,
                    item: playlist.clone(),
                });
            }
        }
    }

    let mut visited: HashSet<&str> = HashSet::new();
    let mut root_items = build_children_for(
        ROOT_PARENT,
        &folders_by_parent,
        &leaves_by_parent,
        &mut visited,
    );
    root_items.extend(unmapped_root);
    root_items
}

fn build_children_for<'a, T: Clone>(
    parent_id: &'a str,
    folders_by_parent: &HashMap<&'a str, Vec<FolderMeta<'a>>>,
    leaves_by_parent: &HashMap<&'a str, Vec<FolderTreeItem<T>>>,
    visited: &mut HashSet<&'a str>,
) -> Vec<FolderTreeItem<T>> {
    let mut items: Vec<FolderTreeItem<T>> = Vec::new();

    if let Some(leaves) = leaves_by_parent.get(parent_id) {
        items.extend(leaves.iter().cloned());
    }

    if let Some(child_folders) = folders_by_parent.get(parent_id) {
        for folder in child_folders {
            if !visited.insert(folder.id) {
                continue;
            }
            let children =
                build_children_for(folder.id, folders_by_parent, leaves_by_parent, visited);
            visited.remove(folder.id);

            if !children.is_empty() {
                items.push(FolderTreeItem::Folder {
                    id: String::from(folder.id),
                    name: String::from(folder.name),
                    order: folder.order,
                    children,
                });
            }
        }
    }

    items.sort_by_key(FolderTreeItem::order);
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_folder_blob;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TestPlaylist {
        uri: String,
        title: String,
    }

    #[test]
    fn builds_ordered_tree_prunes_empty_folders_and_appends_unmapped() {
        let blob = b"spotify:start-group:f_empty:Empty+Outer\r\
            spotify:start-group:f_empty_inner:Empty+Inner\r\
            spotify:end-group:f_empty_inner\r\
            spotify:end-group:f_empty\r\
            spotify:start-group:f_rock:Rock\r\
            spotify:playlist:1111111111111111111111\r\
            spotify:start-group:f_metal:Metal\r\
            spotify:playlist:2222222222222222222222\r\
            spotify:end-group:f_metal\r\
            spotify:end-group:f_rock\r\
            spotify:playlist:3333333333333333333333\r";

        let hierarchy = parse_folder_blob(blob).expect("fixture blob should parse");
        let playlists = vec![
            TestPlaylist {
                uri: String::from("spotify:playlist:9999999999999999999999"),
                title: String::from("Unmapped A"),
            },
            TestPlaylist {
                uri: String::from("spotify:playlist:3333333333333333333333"),
                title: String::from("Root Mapped"),
            },
            TestPlaylist {
                uri: String::from("spotify:user:alice:playlist:2222222222222222222222"),
                title: String::from("Heavy Metal"),
            },
            TestPlaylist {
                uri: String::from("spotify:playlist:1111111111111111111111"),
                title: String::from("Classic Rock"),
            },
            TestPlaylist {
                uri: String::from("spotify:playlist:8888888888888888888888"),
                title: String::from("Unmapped B"),
            },
        ];

        let tree = build_folder_tree(&playlists, &hierarchy, |p| &p.uri);

        assert_eq!(
            tree.len(),
            4,
            "expected f_rock, Root Mapped, Unmapped A, Unmapped B"
        );

        match &tree[0] {
            FolderTreeItem::Folder {
                id,
                name,
                order,
                children,
            } => {
                assert_eq!(id, "f_rock");
                assert_eq!(name, "Rock");
                assert_eq!(*order, 2);
                assert_eq!(children.len(), 2);
                assert!(matches!(
                    &children[0],
                    FolderTreeItem::Leaf { item, .. } if item.title == "Classic Rock"
                ));
                assert!(matches!(
                    &children[1],
                    FolderTreeItem::Folder { id, children, .. }
                        if id == "f_metal" && children.len() == 1
                ));
            }
            other => panic!("expected Folder f_rock at index 0, got {other:?}"),
        }

        assert!(matches!(
            &tree[1],
            FolderTreeItem::Leaf { item, .. } if item.title == "Root Mapped"
        ));
        assert!(matches!(
            &tree[2],
            FolderTreeItem::Leaf { item, .. } if item.title == "Unmapped A"
        ));
        assert!(matches!(
            &tree[3],
            FolderTreeItem::Leaf { item, .. } if item.title == "Unmapped B"
        ));
    }

    #[test]
    fn prunes_all_folders_when_playlist_slice_is_empty() {
        let blob = b"spotify:start-group:f1:Rock\rspotify:playlist:1111111111111111111111\r";
        let hierarchy = parse_folder_blob(blob).expect("fixture blob should parse");
        let empty: Vec<TestPlaylist> = Vec::new();
        let tree = build_folder_tree(&empty, &hierarchy, |p| &p.uri);
        assert!(tree.is_empty(), "empty input should prune all folders");
    }
}
