#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Layout {
    name: String,
    variant: String,
    model: String,
    options: String,
}

impl Layout {
    pub fn new(name: &str, variant: &str, model: &str, options: &str) -> Self {
        Self {
            name: name.into(),
            variant: variant.into(),
            model: model.into(),
            options: options.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn variant(&self) -> &str {
        &self.variant
    }
    pub fn model(&self) -> &str {
        &self.model
    }
    pub fn options(&self) -> &str {
        &self.options
    }

    pub fn to_text<'a>(
        &self,
        strokes: impl IntoIterator<Item = &'a crate::word::Stroke>,
    ) -> Result<String, crate::xkb::ConversionError> {
        crate::xkb::to_text(self, strokes)
    }
}
