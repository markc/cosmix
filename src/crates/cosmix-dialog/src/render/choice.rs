//! Choice dialogs — ComboBox (dropdown), CheckList (multi-select), RadioList (single-select).

use dioxus::prelude::*;

use crate::types::DialogKind;
use crate::window::{complete, exit};
use crate::{DialogAction, DialogData, DialogRequest};

/// Dropdown selection dialog.
#[component]
pub fn ComboBoxDialog(request: DialogRequest) -> Element {
    let DialogKind::ComboBox { ref text, ref items, default, editable } = request.kind else {
        return rsx! {};
    };

    let prompt = text.clone();
    let options = items.clone();
    let mut selected = use_signal(|| {
        default.and_then(|i| items.get(i).cloned()).unwrap_or_else(|| {
            items.first().cloned().unwrap_or_default()
        })
    });

    rsx! {
        div { class: "alert-dialog",
            div { class: "alert-dialog-body-fill",
                div { class: "alert-dialog-title", "{prompt}" }
                if editable {
                    input {
                        class: "alert-dialog-field-input",
                        r#type: "text",
                        value: "{selected}",
                        list: "combo-options",
                        oninput: move |e: FormEvent| selected.set(e.value()),
                    }
                    datalist { id: "combo-options",
                        for opt in options.iter() {
                            option { value: "{opt}" }
                        }
                    }
                } else {
                    select {
                        class: "alert-dialog-field-select",
                        value: "{selected}",
                        onchange: move |e: FormEvent| selected.set(e.value()),
                        for opt in options.iter() {
                            option { value: "{opt}", "{opt}" }
                        }
                    }
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
                        complete(DialogAction::Ok, DialogData::Text(selected.read().clone()));
                        exit();
                    },
                    "OK"
                }
            }
        }
    }
}

/// Multi-select checklist dialog.
#[component]
pub fn CheckListDialog(request: DialogRequest) -> Element {
    let DialogKind::CheckList { ref text, ref items } = request.kind else {
        return rsx! {};
    };

    let prompt = text.clone();
    let mut states = use_signal(|| {
        items.iter().map(|item| (item.key.clone(), item.checked)).collect::<Vec<_>>()
    });
    let labels: Vec<_> = items.iter().map(|i| (i.key.clone(), i.label.clone())).collect();

    rsx! {
        div { class: "alert-dialog",
            div { class: "alert-dialog-body-fill",
                div { class: "alert-dialog-title", "{prompt}" }
                div { class: "alert-dialog-scroll",
                    for (idx, (key, label)) in labels.iter().enumerate() {
                        label { class: "alert-dialog-list-item", key: "{key}",
                            input {
                                r#type: "checkbox",
                                checked: states.read().get(idx).map(|s| s.1).unwrap_or(false),
                                onchange: move |e: FormEvent| {
                                    let checked = e.value() == "true";
                                    states.write()[idx].1 = checked;
                                },
                            }
                            "{label}"
                        }
                    }
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
                        let selected: Vec<String> = states.read().iter()
                            .filter(|(_, checked)| *checked)
                            .map(|(key, _)| key.clone())
                            .collect();
                        complete(DialogAction::Ok, DialogData::Selection(selected));
                        exit();
                    },
                    "OK"
                }
            }
        }
    }
}

/// Single-select radio list dialog.
#[component]
pub fn RadioListDialog(request: DialogRequest) -> Element {
    let DialogKind::RadioList { ref text, ref items } = request.kind else {
        return rsx! {};
    };

    let prompt = text.clone();
    let mut selected = use_signal(|| {
        items.iter()
            .find(|i| i.checked)
            .map(|i| i.key.clone())
            .unwrap_or_default()
    });
    let labels: Vec<_> = items.iter().map(|i| (i.key.clone(), i.label.clone())).collect();

    rsx! {
        div { class: "alert-dialog",
            div { class: "alert-dialog-body-fill",
                div { class: "alert-dialog-title", "{prompt}" }
                div { class: "alert-dialog-scroll",
                    for (key, label) in labels.iter() {
                        label { class: "alert-dialog-list-item", key: "{key}",
                            input {
                                r#type: "radio",
                                name: "radio-group",
                                value: "{key}",
                                checked: *selected.read() == *key,
                                onchange: {
                                    let key = key.clone();
                                    move |_| selected.set(key.clone())
                                },
                            }
                            "{label}"
                        }
                    }
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
                        let sel = selected.read().clone();
                        if sel.is_empty() {
                            complete(DialogAction::Cancel, DialogData::None);
                        } else {
                            complete(DialogAction::Ok, DialogData::Text(sel));
                        }
                        exit();
                    },
                    "OK"
                }
            }
        }
    }
}
