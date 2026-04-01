use dioxus::prelude::*;

#[component]
pub fn Skeleton(#[props(extends=GlobalAttributes)] attributes: Vec<Attribute>) -> Element {
    rsx! {
        document::Style { {include_str!("./style.css")} }
        div { class: "skeleton", ..attributes }
    }
}
