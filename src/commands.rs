//! Command registry and hotkeys.

use std::collections::HashMap;

use egui::{Key, Modifiers, PointerButton};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cat {
    Quick,
    Folder,
    Tabs,
    Panels,
    View,
    Sort,
    Group,
    Inspector,
    Files,
    Selection,
    Layouts,
    Misc,
}

impl Cmd {
    /// The grouping a "Group By …" command selects.
    pub fn group_key(self) -> Option<crate::config::GroupBy> {
        use crate::config::GroupBy as G;
        Some(match self {
            Cmd::GroupNone => G::None,
            Cmd::GroupName => G::Name,
            Cmd::GroupType => G::Type,
            Cmd::GroupDate => G::Modified,
            Cmd::GroupCreated => G::Created,
            Cmd::GroupAccessed => G::Accessed,
            Cmd::GroupSize => G::Size,
            Cmd::GroupItems => G::TotalItems,
            Cmd::GroupUsage => G::Usage,
            Cmd::GroupDeleted => G::Deleted,
            Cmd::GroupOriginal => G::Original,
            Cmd::GroupFree => G::FreeSpace,
            Cmd::GroupFs => G::FileSystem,
            Cmd::GroupFull => G::PercentFull,
            _ => return None,
        })
    }
}

/// Every "Group By …" command, in menu order.
pub const GROUP_CMDS: [Cmd; 14] = [
    Cmd::GroupNone,
    Cmd::GroupName,
    Cmd::GroupSize,
    Cmd::GroupCreated,
    Cmd::GroupDate,
    Cmd::GroupAccessed,
    Cmd::GroupItems,
    Cmd::GroupType,
    Cmd::GroupUsage,
    Cmd::GroupDeleted,
    Cmd::GroupOriginal,
    Cmd::GroupFree,
    Cmd::GroupFs,
    Cmd::GroupFull,
];

impl Cat {
    pub const ORDER: [Cat; 12] = [
        Cat::Quick,
        Cat::Folder,
        Cat::Files,
        Cat::Selection,
        Cat::Tabs,
        Cat::Panels,
        Cat::View,
        Cat::Sort,
        Cat::Group,
        Cat::Inspector,
        Cat::Layouts,
        Cat::Misc,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Cat::Quick => "Quick",
            Cat::Folder => "Navigate",
            Cat::Tabs => "Tabs",
            Cat::Panels => "Panels",
            Cat::View => "View",
            Cat::Sort => "Sort",
            Cat::Group => "Group By",
            Cat::Inspector => "Inspector",
            Cat::Files => "Files",
            Cat::Selection => "Selection",
            Cat::Layouts => "Layouts",
            Cat::Misc => "General",
        }
    }

    /// The name the simulations click its place in the palette by.
    pub fn control(self) -> &'static str {
        match self {
            Cat::Quick => "category Quick",
            Cat::Folder => "category Navigate",
            Cat::Tabs => "category Tabs",
            Cat::Panels => "category Panels",
            Cat::View => "category View",
            Cat::Sort => "category Sort",
            Cat::Group => "category Group By",
            Cat::Inspector => "category Inspector",
            Cat::Files => "category Files",
            Cat::Selection => "category Selection",
            Cat::Layouts => "category Layouts",
            Cat::Misc => "category General",
        }
    }
}

macro_rules! commands {
    ($( $v:ident, $id:literal, $cat:ident, $title:literal, $desc:literal, [$($k:literal),*] ;)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Cmd { $($v),* }
        pub struct Info { pub id: &'static str, pub cat: Cat, pub title: &'static str, pub desc: &'static str, pub keys: &'static [&'static str] }
        pub const ALL: &[Cmd] = &[$(Cmd::$v),*];
        impl Cmd {
            pub fn info(self) -> &'static Info {
                match self { $(Cmd::$v => &Info { id: $id, cat: Cat::$cat, title: $title, desc: $desc, keys: &[$($k),*] }),* }
            }
            pub fn from_id(id: &str) -> Option<Cmd> {
                match id { $($id => Some(Cmd::$v),)* _ => None }
            }
        }
    };
}

commands! {
    GoTo, "goto", Folder, "Go To", "Jump to a place, a recent folder, or type a path with completion.", ["Ctrl+P"];
    AddressBar, "address_bar", Folder, "Edit Path", "Open Go To prefilled with the current folder's path.", ["Ctrl+L", "Alt+D"];
    Back, "back", Folder, "Back", "Go to the previous folder in this tab's history.", ["Alt+Left", "Mouse4"];
    Forward, "forward", Folder, "Forward", "Go to the next folder in this tab's history.", ["Alt+Right", "Mouse5"];
    Up, "up", Folder, "Up", "Go to the parent folder and select the folder you came from.", ["Alt+Up", "Backspace"];
    Home, "home", Folder, "Home", "Go to your home folder.", ["Alt+Home"];
    Timeline, "timeline", Folder, "Timeline", "Files changed in the last month anywhere in your home folder, newest first, by day.", [];
    Computer, "computer", Folder, "Computer", "Show drives and volumes; open an unmounted one to mount it.", [];
    ConnectServer, "connect_server", Folder, "Connect To Server…", "Open a network share (smb, sftp, ftp, WebDAV) by its address.", [];
    Refresh, "refresh", Folder, "Refresh", "Re-read this folder and reload previews, keeping scroll and selection.", ["Ctrl+R", "Alt+R"];
    Activate, "activate", Folder, "Open", "Open the selected folder here or open files with their default app.", ["Enter"];
    OpenInNewTab, "open_in_new_tab", Folder, "Open In New Tab", "Open selected folders in new tabs; files open their containing folder.", ["Ctrl+Enter"];
    OpenInNewWindow, "open_in_new_window", Folder, "Open In New Window", "Open the selected folder (or this one) in a separate window.", ["Ctrl+N"];
    OpenInRightSplit, "open_in_right_split", Folder, "Open In Right Split", "Open the selected folder in a new panel to the right.", ["Ctrl+Alt+Right"];
    OpenInBottomSplit, "open_in_bottom_split", Folder, "Open In Bottom Split", "Open the selected folder in a new panel below.", ["Ctrl+Alt+Down"];
    OpenTerminal, "open_terminal", Folder, "Open Terminal Here", "Start your terminal in the current folder.", ["Ctrl+Shift+Backtick"];
    RecentProjects, "recent_projects", Folder, "Recent Projects", "Open one of the git repositories in your home folder, the ones you were in last first.", ["Ctrl+Alt+P"];
    QuickOpen, "quick_open", Folder, "Quick Open", "Find any file in this project by a few letters of its path.", ["Ctrl+O"];
    RunTask, "run_task", Folder, "Run Task…", "Run a script, target or recipe of this project in your terminal.", ["Ctrl+Shift+B"];
    ToggleBookmark, "toggle_bookmark", Folder, "Toggle Bookmark", "Add or remove the current folder (or selection) from bookmarks.", ["Ctrl+D"];
    OpenLinkTarget, "open_link_target", Folder, "Open Link Target Location", "Go to the folder containing a symlink's target.", [];
    TabNewOrRecent, "path_dropdown", Folder, "Subfolder Menu", "List the subfolders of the current folder for quick descent.", ["Alt+Down"];

    NewTab, "new_tab", Tabs, "New Tab", "Open a new tab.", ["Ctrl+T"];
    CloseTab, "close_tab", Tabs, "Close Tab", "Close the current tab.", ["Ctrl+W", "Ctrl+F4"];
    CloseAllTabs, "close_all_tabs", Tabs, "Close All Tabs", "Close every tab in this panel except a fresh one.", ["Ctrl+Shift+W"];
    CloseOtherTabs, "close_other_tabs", Tabs, "Close Other Tabs", "Close all tabs in this panel except the current one.", [];
    CloseRightTabs, "close_right_tabs", Tabs, "Close Right Tabs", "Close the tabs to the right of the current one.", [];
    CloseLeftTabs, "close_left_tabs", Tabs, "Close Left Tabs", "Close the tabs to the left of the current one.", [];
    OpenTabInWindow, "open_tab_in_window", Tabs, "Open In Window", "Move the current tab into a new window.", [];
    ReopenTab, "reopen_tab", Tabs, "Reopen Closed Tab", "Bring back the most recently closed tab.", ["Ctrl+Shift+T"];
    DuplicateTab, "duplicate_tab", Tabs, "Duplicate Tab", "Open a copy of the current tab next to it.", [];
    NextTab, "next_tab", Tabs, "Next Tab", "Switch to the tab on the right.", ["Ctrl+Tab", "Ctrl+PageDown"];
    PrevTab, "prev_tab", Tabs, "Previous Tab", "Switch to the tab on the left.", ["Ctrl+Shift+Tab", "Ctrl+PageUp"];
    MoveTabLeft, "move_tab_left", Tabs, "Shift Tab Left", "Move the current tab one place left.", ["Ctrl+Shift+PageUp"];
    MoveTabRight, "move_tab_right", Tabs, "Shift Tab Right", "Move the current tab one place right.", ["Ctrl+Shift+PageDown"];
    Tab1, "select_tab_1", Tabs, "Select Tab 1", "Switch to the first tab.", ["Ctrl+1"];
    Tab2, "select_tab_2", Tabs, "Select Tab 2", "Switch to the second tab.", ["Ctrl+2"];
    Tab3, "select_tab_3", Tabs, "Select Tab 3", "Switch to the third tab.", ["Ctrl+3"];
    Tab4, "select_tab_4", Tabs, "Select Tab 4", "Switch to the fourth tab.", ["Ctrl+4"];
    Tab5, "select_tab_5", Tabs, "Select Tab 5", "Switch to the fifth tab.", ["Ctrl+5"];
    Tab6, "select_tab_6", Tabs, "Select Tab 6", "Switch to the sixth tab.", ["Ctrl+6"];
    Tab7, "select_tab_7", Tabs, "Select Tab 7", "Switch to the seventh tab.", ["Ctrl+7"];
    Tab8, "select_tab_8", Tabs, "Select Tab 8", "Switch to the eighth tab.", ["Ctrl+8"];
    LastTab, "select_last_tab", Tabs, "Select Last Tab", "Switch to the rightmost tab.", ["Ctrl+9"];
    TabSearch, "tab_search", Tabs, "Search Tabs", "Find an open or recently closed tab by name.", ["Ctrl+Shift+E"];

    SplitRight, "split_right", Panels, "Split Right", "Split this panel and show the current tab on the right.", ["Ctrl+Shift+S"];
    SplitBottom, "split_bottom", Panels, "Split Bottom", "Split this panel and show the current tab below.", ["Ctrl+S"];
    ClosePanel, "close_panel", Panels, "Close Panel", "Close this panel and all its tabs.", ["Ctrl+Shift+F4"];
    FocusNextPanel, "focus_next_panel", Panels, "Focus Next Panel", "Move keyboard focus to the next panel.", ["Tab"];
    FocusPrevPanel, "focus_prev_panel", Panels, "Focus Previous Panel", "Move keyboard focus to the previous panel.", ["Shift+Tab"];
    CopyToOther, "copy_to_other_panel", Panels, "Copy To Other Panel", "Copy the selection into the folder of the previously focused panel.", ["F5"];
    MoveToOther, "move_to_other_panel", Panels, "Move To Other Panel", "Move the selection into the folder of the previously focused panel.", ["F6"];
    ComparePanels, "compare_panels", Panels, "Compare Panels", "Mark what only one panel has, what is newer and what differs in size.", [];
    SelectDifferences, "select_differences", Panels, "Select Differences", "Select the entries Compare Panels marks in this panel.", [];
    SyncPanels, "sync_to_other_panel", Panels, "Sync To Other Panel", "Copy what is missing or newer here into the other panel's folder.", [];

    ToggleFlatten, "toggle_flatten", View, "Toggle Expand Folder", "Show every file under this folder in one flat, searchable list.", ["Ctrl+E"];
    FocusFilter, "focus_filter", View, "Focus Filter", "Put the cursor in the filter box.", ["Ctrl+F"];
    MeaningSearch, "search_meaning", View, "Search Everywhere", "Find files and folders in your home folder by name, and by what they are about with search by meaning on.", ["Ctrl+K"];
    ContentSearch, "search_contents", View, "Search File Contents", "Switch the filter between file names and the text inside files.", ["Ctrl+Shift+F"];
    SaveSmart, "save_smart_folder", View, "Save As Smart Folder…", "Keep this folder's search in the sidebar, searched again each time it opens.", [];
    NewSmart, "new_smart_folder", View, "New Smart Folder…", "A saved search in the sidebar: a folder and what to find in it, by name, kind, date or size.", [];
    CodeSearch, "search_code", View, "Search Code", "Find text in this project's files, by word, case or regular expression.", ["Ctrl+Alt+F"];
    FindTodos, "find_todos", View, "Find TODOs", "List every TODO, FIXME, HACK and XXX comment in this project.", [];
    ViewPictures, "view_pictures", View, "View Pictures", "Show this folder's pictures full window, from the one under the cursor; Space plays them as a slideshow.", ["F3"];
    TypesFilter, "types_filter", View, "Filter By Type", "Choose which file types are shown.", ["Alt+T"];
    ToggleHidden, "toggle_hidden", View, "Toggle Hidden Files", "Show or hide dotfiles.", ["Ctrl+H"];
    ToggleSystem, "toggle_system", View, "Toggle System Files", "Show or hide backup files and special files (sockets, devices).", [];
    ToggleExtensions, "toggle_extensions", View, "Toggle File Extensions", "Show or hide file name extensions.", [];
    ToggleRecents, "toggle_recents", View, "Toggle Recents", "Mark recently used items with a dot.", [];
    ToggleRecentsOnTop, "toggle_recents_on_top", View, "Toggle Recents On Top", "Sort recently used items above the rest.", [];
    ToggleSidebar, "toggle_sidebar", View, "Toggle Sidebar", "Show or hide the quick access sidebar.", ["Ctrl+B"];
    ZoomIn, "zoom_in", View, "Zoom In", "Step toward bigger icons (also Ctrl+wheel).", ["Ctrl+Equals", "Ctrl+Plus"];
    ZoomOut, "zoom_out", View, "Zoom Out", "Step toward denser views (also Ctrl+wheel).", ["Ctrl+Minus"];
    ViewDetails, "view_details", View, "View Details", "A table with sortable columns.", ["Ctrl+Shift+1", "Ctrl+0"];
    ViewTree, "view_tree", View, "View Tree", "The details table with folders you can unfold in place.", ["Ctrl+Shift+2"];
    ViewList, "view_list", View, "View List", "Compact names flowing down in columns, scrolling sideways.", ["Ctrl+Shift+3"];
    ViewMedium, "view_medium", View, "View Medium Icons", "A grid of medium icons.", ["Ctrl+Shift+4"];
    ViewLarge, "view_large", View, "View Large Icons", "A grid of large thumbnails.", ["Ctrl+Shift+5"];
    ViewXL, "view_xl", View, "View XL Icons", "The biggest thumbnails.", ["Ctrl+Shift+6"];
    GroupNone, "group_none", Group, "Clear Grouping", "List everything in one run, without group headers.", [];
    GroupName, "group_name", Group, "Group By Name", "Alphabetical ranges.", [];
    GroupType, "group_type", Group, "Group By Type", "One group per file type.", [];
    GroupDate, "group_date", Group, "Group By Date Modified", "Today, yesterday, last week… by modification time.", [];
    GroupCreated, "group_created", Group, "Group By Date Created", "Today, yesterday, last week… by creation time.", [];
    GroupAccessed, "group_accessed", Group, "Group By Date Accessed", "Today, yesterday, last week… by last access.", [];
    GroupSize, "group_size", Group, "Group By Size", "Size buckets from empty to huge.", [];
    GroupItems, "group_total_items", Group, "Group By Total Items", "Folders by how many items they contain.", [];
    GroupUsage, "group_folder_usage", Group, "Group By Folder Usage", "By each item's share of this folder's size.", [];
    GroupDeleted, "group_date_deleted", Group, "Group By Date Deleted", "By when items were trashed (Trash only).", [];
    GroupOriginal, "group_original_location", Group, "Group By Original Location", "By the folder items were trashed from (Trash only).", [];
    GroupFree, "group_free_space", Group, "Group By Free Space", "Drives by free space (Computer only).", [];
    GroupFs, "group_file_system", Group, "Group By File System", "Drives by file system (Computer only).", [];
    GroupFull, "group_percent_full", Group, "Group By Percent Full", "Drives by how full they are (Computer only).", [];
    GroupExpandAll, "group_expand_all", Group, "Expand All Groups", "Open every group.", [];
    GroupCollapseAll, "group_collapse_all", Group, "Collapse All Groups", "Close every group, leaving just the headers.", [];
    SavedView, "saved_view_settings", View, "Saved View Settings", "Remember this view for this folder, its subfolders, or everywhere.", [];

    SortName, "sort_name", Sort, "Sort By Name", "Sort by name; repeat to reverse.", ["Ctrl+Alt+1"];
    SortSize, "sort_size", Sort, "Sort By Size", "Sort by size (largest first); repeat to reverse.", ["Ctrl+Alt+2"];
    SortModified, "sort_modified", Sort, "Sort By Date Modified", "Sort by modification time (newest first); repeat to reverse.", ["Ctrl+Alt+3"];
    SortType, "sort_type", Sort, "Sort By Type", "Sort by type; repeat to reverse.", ["Ctrl+Alt+4"];
    SortCreated, "sort_created", Sort, "Sort By Date Created", "Sort by creation time; repeat to reverse.", [];
    SortNameAsc, "sort_name_asc", Sort, "Sort By Name Asc", "Sort A to Z.", [];
    SortNameDesc, "sort_name_desc", Sort, "Sort By Name Desc", "Sort Z to A.", [];
    SortSizeAsc, "sort_size_asc", Sort, "Sort By Size Asc", "Smallest first (folders by their content size).", [];
    SortSizeDesc, "sort_size_desc", Sort, "Sort By Size Desc", "Largest first (folders by their content size).", [];
    SortFullAsc, "sort_percent_full_asc", Sort, "Sort By Percent Full Asc", "Drives with the most free space first (Computer only).", [];
    SortFullDesc, "sort_percent_full_desc", Sort, "Sort By Percent Full Desc", "Fullest drives first (Computer only).", [];
    ToggleFoldersFirst, "toggle_folders_first", Sort, "Toggle Folders First", "Switch between listing folders first or files first.", [];

    ToggleInspector, "toggle_inspector", Inspector, "Toggle Inspector", "Preview the item under the cursor beside the list.", ["Space", "Ctrl+I"];
    InspectorLineNumbers, "inspector_line_numbers", Inspector, "Toggle Inspector Line Numbers", "Show or hide line numbers in text previews.", [];
    InspectorWrap, "inspector_word_wrap", Inspector, "Toggle Inspector Word Wrap", "Wrap long lines in text previews (remembered apart for code and for prose).", [];
    InspectorSource, "inspector_source", Inspector, "Toggle Inspector Source", "Show Markdown, data files and tables as written, or rendered (remembered for each).", [];
    InspectorBlame, "inspector_blame", Inspector, "Toggle Inspector Blame", "Who last changed each line of a file in a git repository, and when.", [];
    InspectorFollow, "inspector_follow", Inspector, "Toggle Inspector Follow", "Keep the preview of a growing file, such as a log, at its end as lines arrive.", [];
    InspectorMinimap, "inspector_minimap", Inspector, "Toggle Inspector Image Minimap", "Show or hide the overview while zoomed into an image.", [];
    GitHistory, "show_git_history", Inspector, "Show Git History", "The commits of this folder's repository as a graph, in the inspector.", [];
    GitBranches, "git_branches", Inspector, "Git Branches", "Switch or create a branch, use the stash, open another work tree.", [];
    OnlyChanged, "only_changed", View, "Show Only Changed Files", "List only what git marks as changed in this folder, and the folders with changes inside; again to show everything.", ["Ctrl+Shift+G"];
    GitChanges, "git_changes", Inspector, "Show Git Changes", "Stage, discard and commit this repository's changes, in the inspector.", [];
    Properties, "properties", Inspector, "Properties", "Details, sizes and permissions of the selection.", ["Alt+Enter"];

    Copy, "copy", Files, "Copy", "Copy the selection to the clipboard.", ["Ctrl+C", "Ctrl+Insert"];
    Cut, "cut", Files, "Cut", "Cut the selection to move it on paste.", ["Ctrl+X"];
    Paste, "paste", Files, "Paste", "Paste files here (or into the single selected folder).", ["Ctrl+V", "Shift+Insert"];
    CopyPath, "copy_path", Files, "Copy As Path", "Copy full paths, quoted when needed; the folder if nothing is selected.", ["Ctrl+Shift+C"];
    CopyPathRaw, "copy_path_raw", Files, "Copy As Path (Raw)", "Copy full paths without any quoting.", [];
    CopyName, "copy_name", Files, "Copy Name", "Copy just the file names.", [];
    CopyPathRelative, "copy_path_relative", Files, "Copy Relative Path", "Copy paths written from the top of the project.", [];
    CopyPathLine, "copy_path_line", Files, "Copy Path With Line", "Copy path:line for the line a search showed in this file.", [];
    CopyCodeBlocks, "copy_code_blocks", Files, "Copy As Code Blocks", "Copy the selected files' text as Markdown code blocks under their paths; a folder gives the files git keeps.", [];
    CopyWebLink, "copy_web_link", Files, "Copy Web Link", "Copy a link to the selection on GitHub, GitLab or Codeberg at this commit, with the line a search showed.", [];
    OpenWebLink, "open_web_link", Files, "Open Web Link", "Open the selection on GitHub, GitLab or Codeberg in your browser, at this commit and the line a search showed.", [];
    Email, "email", Files, "Email", "Attach the selected files to a new email in your mail app.", [];
    Print, "print", Files, "Print…", "Print the selected files on one of your printers.", [];
    RotateLeft, "rotate_left", Files, "Rotate Left", "Turn the selected pictures a quarter to the left, in place; the originals go to the trash.", [];
    RotateRight, "rotate_right", Files, "Rotate Right", "Turn the selected pictures a quarter to the right, in place; the originals go to the trash.", [];
    ResizeHalf, "resize_half", Files, "Resize To Half", "Save a copy of each selected picture at half its size, beside it.", [];
    ResizeQuarter, "resize_quarter", Files, "Resize To A Quarter", "Save a copy of each selected picture at a quarter of its size, beside it.", [];
    ResizeFit, "resize_fit", Files, "Resize To Fit 1920 px", "Save a copy of each selected picture no larger than 1920 pixels on its longer side, beside it.", [];
    ResizeCustom, "resize_custom", Files, "Resize To…", "Save a copy of each selected picture at a size you type: its longer side, width × height, or a percentage.", [];
    CropPicture, "crop_picture", Files, "Crop…", "Drag over the part of the picture to keep in the inspector, then press Enter to save it as a copy.", [];
    SetWallpaper, "set_wallpaper", Files, "Set As Wallpaper", "Make the selected picture the desktop's wallpaper.", [];
    CompareFiles, "compare_files", Files, "Compare Files", "Show how the two selected files differ, in the inspector.", [];
    GitIgnore, "git_ignore", Files, "Add To .gitignore", "Ignore the selected files or folders in this repository's .gitignore.", [];
    GitIgnoreExt, "git_ignore_ext", Files, "Ignore Files Like This", "Ignore every file with the selected file's extension in this repository's .gitignore.", [];
    AddToShelf, "add_to_shelf", Files, "Add To Shelf", "Gather the selection on the shelf, to move or copy it with things from other folders.", ["Alt+S"];
    ShelfMoveHere, "shelf_move_here", Files, "Move Shelf Here", "Move everything on the shelf into this folder.", [];
    ShelfCopyHere, "shelf_copy_here", Files, "Copy Shelf Here", "Copy everything on the shelf into this folder, keeping the shelf.", [];
    ClearShelf, "clear_shelf", Files, "Clear Shelf", "Take everything off the shelf; the files stay where they are.", [];
    Trash, "trash", Files, "Move To Trash", "Move the selection to the trash.", ["Delete"];
    DeletePermanent, "delete_permanent", Files, "Delete Permanently", "Delete the selection without using the trash (asks first).", ["Shift+Delete"];
    EditTags, "edit_tags", Files, "Edit Music Tags…", "Title, artist, album, track, year, genre and cover of the selected sound files; a picture selected with them can be their cover.", [];
    BatchRename, "batch_rename", Files, "Batch Rename…", "Rename the selected items together, numbered or found and replaced, seeing every new name first.", [];
    Rename, "rename", Files, "Rename", "Rename in place; with several items selected, edit all names at once.", ["F2"];
    NewFolder, "new_folder", Files, "New Folder", "Create a folder here and start naming it.", ["Ctrl+Shift+N"];
    NewFile, "new_file", Files, "New File", "Create an empty file here and start naming it.", ["Ctrl+Alt+N"];
    CreateLink, "create_link", Files, "Create Link", "Create symbolic links to the selection.", [];
    Duplicate, "duplicate", Files, "Duplicate", "Make copies of the selection in the same folder.", ["Ctrl+Shift+D"];
    OpenWith, "open_with", Files, "Open With…", "Pick an application to open the selection with.", [];
    OpenInEditor, "open_in_editor", Files, "Open In Editor", "Open the selected files in your code editor (chosen in Options).", ["F4"];
    WhatsUsing, "whats_using", Files, "What's Using This…", "List the programs that have the selection open or work inside it, and stop your own.", [];
    FindDuplicates, "find_duplicates", Files, "Find Duplicates…", "Find files with the same contents under this folder, whatever their names, and trash the extra copies.", [];
    TidyFolder, "tidy_folder", Files, "Tidy Folder…", "Sort this folder's loose files into folders by kind, or its photos into a folder a month by when they were taken, seeing every move first.", [];
    SpaceUsed, "space_used", Files, "Show Space Used", "This folder's contents biggest first, each with its share, to find what fills the disk.", [];
    PreviousVersions, "previous_versions", Files, "Previous Versions…", "Earlier copies of the file kept in the disk's snapshots, to look at and restore.", [];
    CleanBuilds, "clean_build_folders", Files, "Clean Build Folders…", "Find build output and installed packages under this folder and trash the ones you pick.", [];
    ContextMenu, "context_menu", Files, "Context Menu", "Open the action menu at the file cursor.", ["Shift+F10"];
    RestoreTrash, "restore_trash", Files, "Restore From Trash", "Put trashed items back where they came from.", [];
    Undo, "undo", Files, "Undo", "Reverse the last rename, move, copy, create or trash.", ["Ctrl+Z"];
    EmptyTrash, "empty_trash", Files, "Empty Trash", "Permanently remove everything in the trash (asks first).", [];
    ExtractHere, "extract_here", Files, "Extract Here", "Unpack the selected archives (or what is selected inside one) beside them.", [];
    CompressZip, "compress_zip", Files, "Compress To Zip", "Pack the selection into a .zip archive.", [];
    CompressTarGz, "compress_tar_gz", Files, "Compress To Tar.gz", "Pack the selection into a .tar.gz archive.", [];
    CompressTarXz, "compress_tar_xz", Files, "Compress To Tar.xz", "Pack the selection into a .tar.xz archive.", [];
    Compress7z, "compress_7z", Files, "Compress To 7z", "Pack the selection into a .7z archive.", [];
    CompressLocked, "compress_locked", Files, "Compress With Password…", "Pack the selection into a .zip (AES-256) or a .7z that opens only with the password you type; a 7z hides the names too.", [];
    ConvertJpg, "convert_jpg", Files, "Convert To JPG", "Save a JPG copy of each selected picture beside it, upright and on white where it was see-through.", [];
    ConvertPng, "convert_png", Files, "Convert To PNG", "Save a PNG copy of each selected picture beside it.", [];
    ConvertWebp, "convert_webp", Files, "Convert To WebP", "Save a WebP copy of each selected picture beside it.", [];
    ConvertPdf, "convert_pdf", Files, "Convert Pictures To PDF…", "Put the selected pictures into one PDF, a page each, upright and fitted to A4.", [];
    ConvertMp3, "convert_mp3", Files, "Convert To MP3", "Save an MP3 copy of each selected sound file beside it, with its tags and cover.", [];
    ConvertOpus, "convert_opus", Files, "Convert To Opus", "Save an Opus copy of each selected sound file beside it.", [];
    ConvertFlac, "convert_flac", Files, "Convert To FLAC", "Save a lossless FLAC copy of each selected sound file beside it.", [];
    ConvertMp4, "convert_mp4", Files, "Convert To MP4", "Save an MP4 (H.264) copy of each selected video beside it, one that plays almost anywhere.", [];
    ConvertWebm, "convert_webm", Files, "Convert To WebM", "Save a WebM (VP9) copy of each selected video beside it.", [];
    ConvertSoundMp3, "convert_sound_mp3", Files, "Save Sound As MP3", "Save the sound of each selected video as an MP3 beside it.", [];
    MergePdfs, "merge_pdfs", Files, "Merge PDFs…", "Join the selected PDFs into one, in the order they are listed.", [];
    SplitPdf, "split_pdf", Files, "Split PDF Into Pages", "Save every page of the selected PDF as a PDF of its own, in a folder beside it.", [];

    SelectAll, "select_all", Selection, "Select All", "Select every item without scrolling.", ["Ctrl+A"];
    SelectNone, "select_none", Selection, "Select None", "Clear the selection.", ["Ctrl+Shift+A"];
    InvertSelection, "invert_selection", Selection, "Invert Selection", "Select what isn't selected and vice versa.", [];
    ToggleSelection, "toggle_selection", Selection, "Toggle Selection", "Select or unselect the item under the cursor.", ["Ctrl+Space"];

    SaveLayout, "save_layout", Layouts, "Save Layout As…", "Save panels, tabs and window size under a name.", [];
    LoadLayout, "load_layout", Layouts, "Load Layout…", "Switch to a saved layout.", [];
    DeleteLayout, "delete_layout", Layouts, "Delete Layout…", "Remove a saved layout.", [];

    Palette, "palette", Misc, "Command Palette", "Search every command and assign hotkeys.", ["Ctrl+Shift+P"];
    Options, "options", Misc, "Options", "Change appearance, behavior and defaults.", ["Ctrl+Comma"];
    WhatsUsingPort, "whats_using_port", Misc, "What's Using a Port…", "List the network ports programs listen on, whose they are, and stop your own.", [];
    CheckUpdates, "check_updates", Misc, "Check for Updates", "Ask GitHub whether a newer Maia is out, and update from Options.", [];
    ChooseTheme, "choose_theme", Misc, "Choose Theme", "See every color theme side by side, with its code colors, and pick one.", [];
    MainMenu, "main_menu", Misc, "Main Menu", "Open the main menu.", ["F10"];
    QuickMenu, "quick_menu", Misc, "Quick Menu", "Your starred commands in a small menu.", [];
    NewWindow, "new_window", Misc, "New Window", "Open another window.", [];
    Quit, "quit", Misc, "Quit", "Close this window.", ["Ctrl+Q"];
    FontBigger, "font_bigger", Misc, "Increase Font Size", "Make text larger.", ["Ctrl+Shift+Equals"];
    FontSmaller, "font_smaller", Misc, "Decrease Font Size", "Make text smaller.", ["Ctrl+Shift+Minus"];
    ToggleAnimations, "toggle_animations", Misc, "Toggle Animations", "Turn all animations on or off.", [];
    OpenConfig, "open_config", Misc, "Open Config", "Open the settings file in your editor.", [];
    RevealConfig, "reveal_config", Misc, "Reveal Config Folder", "Open the folder that holds Maia's settings.", [];
    ResetConfig, "reset_config", Misc, "Reset Settings", "Restore all options and hotkeys to defaults (asks first).", [];
    Install, "install", Misc, "Install Maia", "Add Maia to your app launcher and ~/.local/bin.", [];
    SetDefault, "set_default_fm", Misc, "Set As Default File Manager", "Make Maia open folders for other apps.", [];
    Uninstall, "uninstall", Misc, "Uninstall Maia", "Remove the launcher entry and installed binary.", [];
    About, "about", Misc, "About Maia", "Version and credits.", [];
}

// ---------------------------------------------------------------- hotkeys

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Input {
    Key(Key),
    Mouse(u8), // 3 = middle, 4 = back, 5 = forward
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub input: Input,
}

pub type Binding = Vec<Chord>;

impl Chord {
    pub fn from_key(key: Key, m: Modifiers) -> Chord {
        Chord { ctrl: m.ctrl || m.command, shift: m.shift, alt: m.alt, input: Input::Key(key) }
    }
    pub fn from_mouse(b: PointerButton, m: Modifiers) -> Option<Chord> {
        let n = match b {
            PointerButton::Middle => 3,
            PointerButton::Extra1 => 4,
            PointerButton::Extra2 => 5,
            _ => return None,
        };
        Some(Chord { ctrl: m.ctrl || m.command, shift: m.shift, alt: m.alt, input: Input::Mouse(n) })
    }
    /// A plain printable key without Ctrl/Alt (subject to the character-key option).
    pub fn is_plain(&self) -> bool {
        !self.ctrl && !self.alt && matches!(self.input, Input::Key(k) if key_is_char(k))
    }
    pub fn has_command_mod(&self) -> bool {
        self.ctrl || self.alt || matches!(self.input, Input::Key(k) if is_fkey(k)) || matches!(self.input, Input::Mouse(_))
    }
}

pub fn key_is_char(k: Key) -> bool {
    let n = k.name();
    n.len() == 1 || matches!(k, Key::Space | Key::Minus | Key::Equals | Key::Comma | Key::Period | Key::Slash | Key::Backtick | Key::Semicolon)
}

fn is_fkey(k: Key) -> bool {
    let n = k.name();
    n.len() >= 2 && n.starts_with('F') && n[1..].chars().all(|c| c.is_ascii_digit())
}

fn key_display(k: Key) -> &'static str {
    match k {
        Key::ArrowLeft => "Left",
        Key::ArrowRight => "Right",
        Key::ArrowUp => "Up",
        Key::ArrowDown => "Down",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        Key::Backtick => "`",
        Key::Equals => "=",
        Key::Plus => "+",
        Key::Minus => "-",
        Key::Comma => ",",
        Key::Period => ".",
        Key::Slash => "/",
        _ => k.name(),
    }
}

impl std::fmt::Display for Chord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.ctrl {
            write!(f, "Ctrl+")?;
        }
        if self.alt {
            write!(f, "Alt+")?;
        }
        if self.shift {
            write!(f, "Shift+")?;
        }
        match self.input {
            Input::Key(k) => write!(f, "{}", key_display(k)),
            Input::Mouse(3) => write!(f, "MouseMiddle"),
            Input::Mouse(n) => write!(f, "Mouse{n}"),
        }
    }
}

pub fn binding_to_string(b: &Binding) -> String {
    b.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ")
}

fn parse_key(name: &str) -> Option<Key> {
    let alias = match name {
        "Left" => "ArrowLeft",
        "Right" => "ArrowRight",
        "Up" => "ArrowUp",
        "Down" => "ArrowDown",
        "PgUp" => "PageUp",
        "PgDn" => "PageDown",
        "Del" => "Delete",
        "Ins" => "Insert",
        "Esc" => "Escape",
        "Return" => "Enter",
        "`" => "Backtick",
        "=" => "Equals",
        "+" => "Plus",
        "-" => "Minus",
        "," => "Comma",
        "." => "Period",
        "/" => "Slash",
        other => other,
    };
    Key::from_name(alias)
}

pub fn parse_chord(s: &str) -> Option<Chord> {
    let s = s.trim();
    let mut c = Chord { ctrl: false, shift: false, alt: false, input: Input::Key(Key::A) };
    // Split on '+' but allow a trailing "+" key ("Ctrl++").
    let mut parts: Vec<&str> = s.split('+').collect();
    if s.ends_with("++") {
        parts.pop();
        parts.pop();
        parts.push("+");
    }
    let last = parts.pop()?;
    for m in parts {
        match m.trim() {
            "Ctrl" | "Control" | "Cmd" => c.ctrl = true,
            "Shift" => c.shift = true,
            "Alt" => c.alt = true,
            _ => return None,
        }
    }
    c.input = match last.trim() {
        "MouseMiddle" | "Mouse3" => Input::Mouse(3),
        "Mouse4" => Input::Mouse(4),
        "Mouse5" => Input::Mouse(5),
        k => Input::Key(parse_key(k)?),
    };
    Some(c)
}

pub fn parse_binding(s: &str) -> Option<Binding> {
    let b: Option<Vec<Chord>> = s.split(", ").map(parse_chord).collect();
    b.filter(|v| !v.is_empty())
}

pub struct Keymap {
    pub map: HashMap<Cmd, Vec<Binding>>,
}

impl Keymap {
    pub fn build(overrides: &std::collections::BTreeMap<String, Vec<String>>) -> Keymap {
        let mut map = HashMap::new();
        for &c in ALL {
            let info = c.info();
            let src: Vec<String> = match overrides.get(info.id) {
                Some(v) => v.clone(),
                None => info.keys.iter().map(|s| s.to_string()).collect(),
            };
            map.insert(c, src.iter().filter_map(|s| parse_binding(s)).collect());
        }
        Keymap { map }
    }

    pub fn bindings(&self, c: Cmd) -> &[Binding] {
        self.map.get(&c).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn label(&self, c: Cmd) -> Option<String> {
        self.bindings(c).first().map(binding_to_string)
    }

    /// Commands whose binding equals or starts with `seq`.
    pub fn lookup(&self, seq: &[Chord]) -> (Option<Cmd>, bool) {
        let mut exact = None;
        let mut longer = false;
        for &c in ALL {
            for b in self.bindings(c) {
                if b.len() >= seq.len() && b[..seq.len()] == *seq {
                    if b.len() == seq.len() {
                        if exact.is_none() {
                            exact = Some(c);
                        }
                    } else {
                        longer = true;
                    }
                }
            }
        }
        (exact, longer)
    }

    pub fn conflicts(&self, b: &Binding, except: Cmd) -> Option<Cmd> {
        ALL.iter().copied().find(|&c| c != except && self.bindings(c).iter().any(|x| x == b))
    }
}
