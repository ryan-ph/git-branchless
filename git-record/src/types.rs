use std::{borrow::Cow, path::PathBuf};
pub struct RecordState<'a> {
    pub file_states: Vec<(PathBuf, FileState<'a>)>,
pub type FileMode = usize;

pub struct FileState<'a> {
    /// The Unix file mode of the file, if available.
    ///
    /// This value is not directly modified by the UI; instead, construct a
    /// [`Section::FileMode`] and use the [`FileState::get_file_mode`] function
    /// to read a user-provided updated to the file mode function to read a
    /// user-provided updated to the file mode
    pub file_mode: Option<FileMode>,
    /// The set of [`Section`]s inside the file.
    pub sections: Vec<Section<'a>>,
}
impl FileState<'_> {
    /// An absent file.
    pub fn absent() -> Self {
        unimplemented!("FileState::absent")
    }
    /// A binary file.
    pub fn binary() -> Self {
        unimplemented!("FileState::binary")
    }
    pub fn count_changed_sections(&self) -> usize {
        let Self {
            file_mode: _,
            sections,
        } = self;
        sections
            .iter()
            .filter(|section| match section {
                Section::Unchanged { .. } => false,
                Section::Changed { .. } => true,
                Section::FileMode { .. } => {
                    unimplemented!("count_changed_sections for Section::FileMode")
                }
            })
            .count()
    }

    /// Get the new Unix file mode. If the user selected a
    /// [`Section::FileMode`], then returns that file mode. Otherwise, returns
    /// the `file_mode` value that this [`FileState`] was constructed with.
    pub fn get_file_mode(&self) -> Option<FileMode> {
        let Self {
            file_mode,
            sections,
        } = self;
        sections
            .iter()
            .find_map(|section| match section {
                Section::Unchanged { .. }
                | Section::Changed { .. }
                | Section::FileMode {
                    is_selected: false,
                    before: _,
                    after: _,
                } => None,

                Section::FileMode {
                    is_selected: true,
                    before: _,
                    after,
                } => Some(*after),
            })
            .or(*file_mode)
        let Self {
            file_mode: _,
            sections,
        } = self;
        for section in sections {
            match section {
                Section::Unchanged { contents } => {
                    for line in contents {
                        acc_selected.push_str(line);
                        acc_unselected.push_str(line);
                    }
                }
                Section::Changed { before, after } => {
                    for SectionChangedLine { is_selected, line } in before {
                        // Note the inverted condition here.
                        if !*is_selected {
                            acc_selected.push_str(line);
                        } else {
                            acc_unselected.push_str(line);
                    }

                    for SectionChangedLine { is_selected, line } in after {
                        if *is_selected {
                            acc_selected.push_str(line);
                        } else {
                            acc_unselected.push_str(line);
                Section::FileMode {
                    is_selected: _,
                    before: _,
                    after: _,
                } => {
                    unimplemented!("get_selected_contents for Section::FileMode");
                }
pub enum Section<'a> {
        contents: Vec<Cow<'a, str>>,
        before: Vec<SectionChangedLine<'a>>,
        after: Vec<SectionChangedLine<'a>>,
    },

    /// The Unix file mode of the file changed, and the user needs to select
    /// whether to accept that mode change or not.
    FileMode {
        /// Whether or not the file mode change was accepted.
        is_selected: bool,

        /// The old file mode.
        before: FileMode,

        /// The new file mode.
        after: FileMode,
/// A changed line inside a `Section`.
pub struct SectionChangedLine<'a> {
    pub line: Cow<'a, str>,
}

impl<'a> SectionChangedLine<'a> {
    /// Make a copy of this [`SectionChangedLine`] that borrows the content of
    /// the line from the original.
    pub fn borrow_line(&'a self) -> Self {
        let Self { is_selected, line } = self;
        Self {
            is_selected: *is_selected,
            line: Cow::Borrowed(line),
        }
    }