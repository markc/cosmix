//! Form dialog — multi-field structured input.

use std::collections::BTreeMap;

use dioxus::prelude::*;

use crate::types::{DialogKind, FieldKind};
use crate::window::{complete, exit};
use crate::{DialogAction, DialogData, DialogRequest};

#[component]
pub fn FormDialog(request: DialogRequest) -> Element {
    let DialogKind::Form { ref text, ref fields } = request.kind else {
        return rsx! {};
    };

    let prompt = text.clone();
    let field_defs = fields.clone();

    let values = use_signal(|| {
        let mut map = BTreeMap::new();
        for f in fields.iter() {
            let default = match &f.kind {
                FieldKind::Text { default, .. } => default.clone().unwrap_or_default(),
                FieldKind::Password => String::new(),
                FieldKind::Number { default, .. } => {
                    default.map(|n| n.to_string()).unwrap_or_default()
                }
                FieldKind::Toggle { default } => default.to_string(),
                FieldKind::Select { items, default } => {
                    default
                        .and_then(|i| items.get(i).cloned())
                        .unwrap_or_else(|| items.first().cloned().unwrap_or_default())
                }
                FieldKind::TextArea { default, .. } => default.clone().unwrap_or_default(),
                FieldKind::Label { .. } | FieldKind::Separator => String::new(),
            };
            map.insert(f.id.clone(), default);
        }
        map
    });

    rsx! {
        div { class: "alert-dialog",
            div { class: "alert-dialog-body-fill",
                if !prompt.is_empty() {
                    div { class: "alert-dialog-title", "{prompt}" }
                }
                for field in field_defs.iter() {
                    {render_field(field, &values)}
                }
            }
            div { class: "alert-dialog-actions",
                div {
                    class: "alert-dialog-cancel",
                    onclick: move |_| {
                        complete(DialogAction::Cancel, DialogData::None);
                        exit();
                    },
                    "Cancel"
                }
                div {
                    class: "alert-dialog-action",
                    onclick: move |_| {
                        complete(DialogAction::Ok, DialogData::Form(values.read().clone()));
                        exit();
                    },
                    "OK"
                }
            }
        }
    }
}

fn render_field(
    field: &crate::types::FormField,
    values: &Signal<BTreeMap<String, String>>,
) -> Element {
    let id = field.id.clone();
    let label = field.label.clone();
    let required = field.required;
    let help = field.help.clone();
    let mut values = *values;

    match &field.kind {
        FieldKind::Separator => rsx! {
            div { style: "height:1px; background:rgba(128,128,128,0.3); margin:0.25rem 0" }
        },

        FieldKind::Label { text } => rsx! {
            div { class: "alert-dialog-field-help", style: "padding:0.25rem 0", "{text}" }
        },

        FieldKind::Text { placeholder, .. } => {
            let ph = placeholder.clone().unwrap_or_default();
            let current = values.read().get(&id).cloned().unwrap_or_default();
            rsx! {
                div { class: "alert-dialog-field",
                    label { class: "alert-dialog-field-label",
                        "{label}"
                        if required { span { style: "color:var(--danger,#ef4444); margin-left:0.125rem", "*" } }
                    }
                    input {
                        class: "alert-dialog-field-input",
                        r#type: "text",
                        placeholder: "{ph}",
                        value: "{current}",
                        oninput: { let id = id.clone(); move |e: FormEvent| { values.write().insert(id.clone(), e.value()); } },
                    }
                    if let Some(help) = &help {
                        div { class: "alert-dialog-field-help", "{help}" }
                    }
                }
            }
        }

        FieldKind::Password => {
            let current = values.read().get(&id).cloned().unwrap_or_default();
            rsx! {
                div { class: "alert-dialog-field",
                    label { class: "alert-dialog-field-label",
                        "{label}"
                        if required { span { style: "color:var(--danger,#ef4444); margin-left:0.125rem", "*" } }
                    }
                    input {
                        class: "alert-dialog-field-input",
                        r#type: "password",
                        value: "{current}",
                        oninput: { let id = id.clone(); move |e: FormEvent| { values.write().insert(id.clone(), e.value()); } },
                    }
                    if let Some(help) = &help {
                        div { class: "alert-dialog-field-help", "{help}" }
                    }
                }
            }
        }

        FieldKind::Number { min, max, step, .. } => {
            let current = values.read().get(&id).cloned().unwrap_or_default();
            let min_str = min.map(|v| v.to_string()).unwrap_or_default();
            let max_str = max.map(|v| v.to_string()).unwrap_or_default();
            let step_str = step.map(|v| v.to_string()).unwrap_or_else(|| "any".to_string());
            rsx! {
                div { class: "alert-dialog-field",
                    label { class: "alert-dialog-field-label",
                        "{label}"
                        if required { span { style: "color:var(--danger,#ef4444); margin-left:0.125rem", "*" } }
                    }
                    input {
                        class: "alert-dialog-field-input",
                        r#type: "number",
                        value: "{current}",
                        min: "{min_str}",
                        max: "{max_str}",
                        step: "{step_str}",
                        oninput: { let id = id.clone(); move |e: FormEvent| { values.write().insert(id.clone(), e.value()); } },
                    }
                    if let Some(help) = &help {
                        div { class: "alert-dialog-field-help", "{help}" }
                    }
                }
            }
        }

        FieldKind::Toggle { .. } => {
            let current = values.read().get(&id).cloned().unwrap_or_default();
            let checked = current == "true";
            rsx! {
                div { class: "alert-dialog-field",
                    label { class: "alert-dialog-list-item", style: "padding:0",
                        input {
                            r#type: "checkbox",
                            checked: checked,
                            onchange: { let id = id.clone(); move |e: FormEvent| {
                                let val = if e.value() == "true" { "true" } else { "false" };
                                values.write().insert(id.clone(), val.to_string());
                            } },
                        }
                        span { class: "alert-dialog-field-label", "{label}" }
                    }
                    if let Some(help) = &help {
                        div { class: "alert-dialog-field-help", "{help}" }
                    }
                }
            }
        }

        FieldKind::Select { items, .. } => {
            let options = items.clone();
            let current = values.read().get(&id).cloned().unwrap_or_default();
            rsx! {
                div { class: "alert-dialog-field",
                    label { class: "alert-dialog-field-label",
                        "{label}"
                        if required { span { style: "color:var(--danger,#ef4444); margin-left:0.125rem", "*" } }
                    }
                    select {
                        class: "alert-dialog-field-select",
                        value: "{current}",
                        onchange: { let id = id.clone(); move |e: FormEvent| { values.write().insert(id.clone(), e.value()); } },
                        for opt in options.iter() {
                            option { value: "{opt}", "{opt}" }
                        }
                    }
                    if let Some(help) = &help {
                        div { class: "alert-dialog-field-help", "{help}" }
                    }
                }
            }
        }

        FieldKind::TextArea { rows, .. } => {
            let current = values.read().get(&id).cloned().unwrap_or_default();
            let row_count = *rows;
            rsx! {
                div { class: "alert-dialog-field",
                    label { class: "alert-dialog-field-label",
                        "{label}"
                        if required { span { style: "color:var(--danger,#ef4444); margin-left:0.125rem", "*" } }
                    }
                    textarea {
                        class: "alert-dialog-field-textarea",
                        rows: "{row_count}",
                        value: "{current}",
                        oninput: { let id = id.clone(); move |e: FormEvent| { values.write().insert(id.clone(), e.value()); } },
                    }
                    if let Some(help) = &help {
                        div { class: "alert-dialog-field-help", "{help}" }
                    }
                }
            }
        }
    }
}
