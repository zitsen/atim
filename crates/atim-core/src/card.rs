// ── Structured Card model ──
//
// Mirrors cc-connect's `core/card.go`: a renderer-agnostic rich message that
// each IM adapter (Feishu, Telegram, …) renders to its native card/button
// format. Replace the thin `Button { text, callback_data }` keyboard model:
// buttons now carry an explicit visual variant and rows a layout hint, and
// richer elements (lists, dividers, dropdowns, footnotes) are representable.

/// Visual variant of a button. Feishu maps to `primary`/`danger`/`default`;
/// Telegram inline keyboards don't color buttons, so the variant is ignored there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonVariant {
    Default,
    Primary,
    Danger,
}

impl ButtonVariant {
    /// Feishu `type` value for this variant.
    pub fn feishu_type(&self) -> &'static str {
        match self {
            ButtonVariant::Default => "default",
            ButtonVariant::Primary => "primary",
            ButtonVariant::Danger => "danger",
        }
    }
}

/// How a button row is laid out on platforms that support rich layouts (Feishu).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionLayout {
    /// Plain horizontal row of buttons (`action` container).
    Row,
    /// Each button takes equal width; two buttons split the row (`column_set` + bisect).
    EqualColumns,
}

/// Colored title bar at the top of a card.
#[derive(Debug, Clone)]
pub struct CardHeader {
    pub title: String,
    /// Feishu card template color: blue/red/orange/green/grey/turquoise/yellow/carmine…
    pub color: String,
}

/// A single clickable button.
#[derive(Debug, Clone)]
pub struct CardButton {
    pub text: String,
    pub variant: ButtonVariant,
    /// Callback data delivered on press (atim's `cb:*`/`ui:*` payload).
    pub value: String,
}

impl CardButton {
    pub fn default(text: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            variant: ButtonVariant::Default,
            value: value.into(),
        }
    }

    pub fn primary(text: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            variant: ButtonVariant::Primary,
            value: value.into(),
        }
    }

    pub fn danger(text: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            variant: ButtonVariant::Danger,
            value: value.into(),
        }
    }
}

/// A single content element inside a card.
#[derive(Debug, Clone)]
pub enum CardElement {
    /// Markdown-formatted paragraph.
    Markdown(String),
    /// Horizontal rule.
    Divider,
    /// A row of buttons.
    Actions {
        buttons: Vec<CardButton>,
        layout: ActionLayout,
    },
    /// A row with description text on the left and a button on the right.
    ListItem {
        text: String,
        btn_text: String,
        btn_variant: ButtonVariant,
        btn_value: String,
    },
    /// Small footnote text at the bottom.
    Note(String),
    /// Dropdown selector.
    Select {
        placeholder: String,
        options: Vec<(String, String)>, // (label, value)
        init_value: Option<String>,
    },
}

/// A rich card message: optional colored header + ordered elements.
#[derive(Debug, Clone)]
pub struct Card {
    pub header: Option<CardHeader>,
    pub elements: Vec<CardElement>,
}

impl Card {
    pub fn builder() -> CardBuilder {
        CardBuilder::new()
    }
}

impl Default for CardBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Fluent constructor for [`Card`].
pub struct CardBuilder {
    card: Card,
}

impl CardBuilder {
    pub fn new() -> Self {
        Self {
            card: Card {
                header: None,
                elements: Vec::new(),
            },
        }
    }

    pub fn header(mut self, title: impl Into<String>, color: impl Into<String>) -> Self {
        self.card.header = Some(CardHeader {
            title: title.into(),
            color: color.into(),
        });
        self
    }

    pub fn markdown(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        if !text.is_empty() {
            self.card.elements.push(CardElement::Markdown(text));
        }
        self
    }

    pub fn divider(mut self) -> Self {
        self.card.elements.push(CardElement::Divider);
        self
    }

    /// Append a button row (plain horizontal layout).
    pub fn actions(mut self, buttons: Vec<CardButton>) -> Self {
        if !buttons.is_empty() {
            self.card.elements.push(CardElement::Actions {
                buttons,
                layout: ActionLayout::Row,
            });
        }
        self
    }

    /// Append a button row where each button takes equal width.
    pub fn actions_equal(mut self, buttons: Vec<CardButton>) -> Self {
        if !buttons.is_empty() {
            self.card.elements.push(CardElement::Actions {
                buttons,
                layout: ActionLayout::EqualColumns,
            });
        }
        self
    }

    /// Append a list row: description text left, button right.
    pub fn list_item(
        mut self,
        text: impl Into<String>,
        btn_text: impl Into<String>,
        btn_variant: ButtonVariant,
        btn_value: impl Into<String>,
    ) -> Self {
        self.card.elements.push(CardElement::ListItem {
            text: text.into(),
            btn_text: btn_text.into(),
            btn_variant,
            btn_value: btn_value.into(),
        });
        self
    }

    pub fn note(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        if !text.is_empty() {
            self.card.elements.push(CardElement::Note(text));
        }
        self
    }

    pub fn select(
        mut self,
        placeholder: impl Into<String>,
        options: Vec<(String, String)>,
        init_value: Option<String>,
    ) -> Self {
        self.card.elements.push(CardElement::Select {
            placeholder: placeholder.into(),
            options,
            init_value,
        });
        self
    }

    pub fn build(self) -> Card {
        self.card
    }
}
