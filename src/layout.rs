#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub name: String,
    pub variant: String,
    pub model: String,
    pub options: String,
}

impl Layout {
    pub fn to_text<'a>(
        &self,
        strokes: impl IntoIterator<Item = &'a crate::word::Stroke>,
    ) -> Result<String, crate::xkb::ConversionError> {
        crate::xkb::to_text(self, strokes)
    }
}
