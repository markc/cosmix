//! Component preview app for cosmix-lib-ui.
//!
//! Demonstrates all official dx-components and custom cosmix components.
//! Uses the standard cosmix app shell: MenuBar on top, content below.
//! Launch with: cd src/crates/cosmix-preview && dx serve

use dioxus::prelude::*;
use dioxus_primitives::scroll_area::ScrollDirection;
use dioxus_primitives::slider::SliderValue;
use dioxus_primitives::ContentSide;
use time::{Date, UtcDateTime};

// dx-components
use cosmix_ui::dx_components::accordion::*;
use cosmix_ui::dx_components::alert_dialog::*;
use cosmix_ui::dx_components::aspect_ratio::*;
use cosmix_ui::dx_components::avatar::*;
use cosmix_ui::dx_components::badge::*;
use cosmix_ui::dx_components::button::*;
use cosmix_ui::dx_components::calendar::*;
use cosmix_ui::dx_components::card::*;
use cosmix_ui::dx_components::checkbox::*;
use cosmix_ui::dx_components::collapsible::*;
use cosmix_ui::dx_components::context_menu::*;
use cosmix_ui::dx_components::date_picker::*;
use cosmix_ui::dx_components::dialog::*;
use cosmix_ui::dx_components::drag_and_drop_list::*;
use cosmix_ui::dx_components::dropdown_menu::*;
use cosmix_ui::dx_components::hover_card::*;
use cosmix_ui::dx_components::input::*;
use cosmix_ui::dx_components::label::*;
use cosmix_ui::dx_components::menubar as dx_menubar;
use cosmix_ui::dx_components::pagination::*;
use cosmix_ui::dx_components::popover::*;
use cosmix_ui::dx_components::progress::*;
use cosmix_ui::dx_components::radio_group::*;
use cosmix_ui::dx_components::scroll_area::*;
use cosmix_ui::dx_components::select::*;
use cosmix_ui::dx_components::separator::*;
use cosmix_ui::dx_components::sheet::*;
use cosmix_ui::dx_components::sidebar::*;
use cosmix_ui::dx_components::skeleton::*;
use cosmix_ui::dx_components::slider::*;
use cosmix_ui::dx_components::switch::*;
use cosmix_ui::dx_components::tabs::*;
use cosmix_ui::dx_components::textarea::*;
#[allow(unused_imports)]
use cosmix_ui::dx_components::toast::*;
use cosmix_ui::dx_components::toggle::*;
use cosmix_ui::dx_components::toggle_group::*;
use cosmix_ui::dx_components::toolbar::*;
use cosmix_ui::dx_components::tooltip::*;
use cosmix_ui::dx_components::virtual_list::*;

// Cosmix menu system
use cosmix_ui::app_init::{use_theme_css, THEME};
use cosmix_ui::menu::{
    action, menubar, separator, standard_file_menu, standard_help_menu, submenu, MenuBar,
};

// Existing cosmix components
use cosmix_ui::components::{DataColumn, DataTable};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    cosmix_ui::app_init::launch_desktop("cosmix-preview", 1200.0, 800.0, app);
}

fn app() -> Element {
    use_theme_css();
    let mut selected = use_signal(|| "button".to_string());
    let mut show_about = use_signal(|| false);

    let app_menu = menubar(vec![
        standard_file_menu(vec![]),
        submenu("Display", vec![
            action("accordion", "Accordion"),
            action("alert_dialog", "Alert Dialog"),
            action("aspect_ratio", "Aspect Ratio"),
            action("avatar", "Avatar"),
            action("badge", "Badge"),
            action("button", "Button"),
            action("calendar", "Calendar"),
            action("card", "Card"),
            action("checkbox", "Checkbox"),
            action("collapsible", "Collapsible"),
            action("skeleton", "Skeleton"),
            action("separator_demo", "Separator"),
        ]),
        submenu("Input", vec![
            action("input", "Input"),
            action("textarea", "Textarea"),
            action("checkbox_demo", "Checkbox"),
            action("radio_group", "Radio Group"),
            action("select", "Select"),
            action("slider", "Slider"),
            action("switch", "Switch"),
            action("toggle", "Toggle"),
            action("toggle_group", "Toggle Group"),
            action("date_picker", "Date Picker"),
        ]),
        submenu("Overlay", vec![
            action("alert_dialog_demo", "Alert Dialog"),
            action("context_menu", "Context Menu"),
            action("dialog", "Dialog"),
            action("dropdown_menu", "Dropdown Menu"),
            action("hover_card", "Hover Card"),
            action("popover", "Popover"),
            action("sheet", "Sheet"),
            action("toast", "Toast"),
            action("tooltip", "Tooltip"),
        ]),
        submenu("Layout", vec![
            action("sidebar", "Sidebar"),
            action("tabs", "Tabs"),
            action("toolbar", "Toolbar"),
            action("menubar_demo", "Menubar"),
            action("pagination", "Pagination"),
            action("scroll_area", "Scroll Area"),
            action("virtual_list", "Virtual List"),
            action("drag_and_drop_list", "Drag & Drop List"),
            action("progress", "Progress"),
        ]),
        submenu("Custom", vec![
            action("data_table", "DataTable"),
        ]),
        submenu("Theme", vec![
            action("theme_light", "Light Mode"),
            action("theme_dark", "Dark Mode"),
        ]),
        standard_help_menu("cosmix-preview"),
    ]);

    *cosmix_ui::menu::MENU_DEF.write() = Some(app_menu.clone());

    rsx! {
        document::Stylesheet { href: asset!("/assets/tailwind.css") }

        // Standard cosmix app shell: menu bar + content
        div { class: "flex flex-col w-full h-full",

            // Menu bar (always on top)
            MenuBar {
                menu: app_menu,
                on_action: move |id: String| match id.as_str() {
                    "quit" => dioxus::desktop::window().close(),
                    "about" => show_about.set(true),
                    "theme_light" => { THEME.write().dark = false; },
                    "theme_dark" => { THEME.write().dark = true; },
                    other => selected.set(other.replace("_demo", "").to_string()),
                },
            }

            // Content area (fills remaining space below menu bar)
            div { class: "flex-1 overflow-y-auto p-6",
                match selected.read().as_str() {
                    "accordion" => rsx! { DemoSection { title: "Accordion", DemoAccordion {} } },
                    "alert_dialog" => rsx! { DemoSection { title: "Alert Dialog", DemoAlertDialog {} } },
                    "aspect_ratio" => rsx! { DemoSection { title: "Aspect Ratio", DemoAspectRatio {} } },
                    "avatar" => rsx! { DemoSection { title: "Avatar", DemoAvatar {} } },
                    "badge" => rsx! { DemoSection { title: "Badge", DemoBadge {} } },
                    "button" => rsx! { DemoSection { title: "Button", DemoButton {} } },
                    "calendar" => rsx! { DemoSection { title: "Calendar", DemoCalendar {} } },
                    "card" => rsx! { DemoSection { title: "Card", DemoCard {} } },
                    "checkbox" => rsx! { DemoSection { title: "Checkbox", DemoCheckbox {} } },
                    "collapsible" => rsx! { DemoSection { title: "Collapsible", DemoCollapsible {} } },
                    "context_menu" => rsx! { DemoSection { title: "Context Menu", DemoContextMenu {} } },
                    "date_picker" => rsx! { DemoSection { title: "Date Picker", DemoDatePicker {} } },
                    "dialog" => rsx! { DemoSection { title: "Dialog", DemoDialog {} } },
                    "drag_and_drop_list" => rsx! { DemoSection { title: "Drag & Drop List", DemoDragAndDropList {} } },
                    "dropdown_menu" => rsx! { DemoSection { title: "Dropdown Menu", DemoDropdownMenu {} } },
                    "hover_card" => rsx! { DemoSection { title: "Hover Card", DemoHoverCard {} } },
                    "input" => rsx! { DemoSection { title: "Input", DemoInput {} } },
                    "label" => rsx! { DemoSection { title: "Label", DemoLabel {} } },
                    "menubar" => rsx! { DemoSection { title: "Menubar (dx-component)", DemoMenubar {} } },
                    "pagination" => rsx! { DemoSection { title: "Pagination", DemoPagination {} } },
                    "popover" => rsx! { DemoSection { title: "Popover", DemoPopover {} } },
                    "progress" => rsx! { DemoSection { title: "Progress", DemoProgress {} } },
                    "radio_group" => rsx! { DemoSection { title: "Radio Group", DemoRadioGroup {} } },
                    "scroll_area" => rsx! { DemoSection { title: "Scroll Area", DemoScrollArea {} } },
                    "select" => rsx! { DemoSection { title: "Select", DemoSelect {} } },
                    "separator" => rsx! { DemoSection { title: "Separator", DemoSeparator {} } },
                    "sheet" => rsx! { DemoSection { title: "Sheet", DemoSheet {} } },
                    "sidebar" => rsx! { DemoSection { title: "Sidebar", DemoSidebar {} } },
                    "skeleton" => rsx! { DemoSection { title: "Skeleton", DemoSkeleton {} } },
                    "slider" => rsx! { DemoSection { title: "Slider", DemoSlider {} } },
                    "switch" => rsx! { DemoSection { title: "Switch", DemoSwitch {} } },
                    "tabs" => rsx! { DemoSection { title: "Tabs", DemoTabs {} } },
                    "textarea" => rsx! { DemoSection { title: "Textarea", DemoTextarea {} } },
                    "toast" => rsx! { DemoSection { title: "Toast", DemoToast {} } },
                    "toggle" => rsx! { DemoSection { title: "Toggle", DemoToggle {} } },
                    "toggle_group" => rsx! { DemoSection { title: "Toggle Group", DemoToggleGroup {} } },
                    "toolbar" => rsx! { DemoSection { title: "Toolbar", DemoToolbar {} } },
                    "tooltip" => rsx! { DemoSection { title: "Tooltip", DemoTooltip {} } },
                    "virtual_list" => rsx! { DemoSection { title: "Virtual List", DemoVirtualList {} } },
                    "data_table" => rsx! { DemoSection { title: "DataTable (custom)", DemoDataTable {} } },
                    _ => rsx! { DemoSection { title: "Button", DemoButton {} } },
                }
            }
        }

        // About dialog
        if show_about() {
            DialogRoot { open: true, on_open_change: move |v| show_about.set(v),
                DialogContent {
                    button { class: "dialog-close", r#type: "button", aria_label: "Close",
                        onclick: move |_| show_about.set(false), "\u{00d7}"
                    }
                    DialogTitle { "About cosmix-preview" }
                    DialogDescription {
                        div { class: "flex flex-col gap-2 mt-2",
                            p { "Component preview and testing app for cosmix-lib-ui." }
                            p { class: "text-sm text-fg-muted",
                                "Dioxus 0.7 + Tailwind CSS v4 + dioxus-primitives"
                            }
                            p { class: "text-sm text-fg-muted",
                                "38 official dx-components + custom cosmix components"
                            }
                            p { class: "text-xs text-fg-muted mt-2",
                                "cosmix \u{00a9} 2026 Mark Constable \u{2022} MIT License"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn DemoSection(title: String, children: Element) -> Element {
    rsx! {
        h2 { class: "mb-4 text-lg text-fg-primary", "{title}" }
        div { class: "flex flex-col gap-4", {children} }
    }
}

// ── Official component demos ──

#[component]
fn DemoButton() -> Element {
    rsx! {
        div { class: "flex gap-2 flex-wrap",
            Button { "Primary" }
            Button { variant: ButtonVariant::Secondary, "Secondary" }
            Button { variant: ButtonVariant::Destructive, "Destructive" }
            Button { variant: ButtonVariant::Outline, "Outline" }
            Button { variant: ButtonVariant::Ghost, "Ghost" }
        }
    }
}

#[component]
fn DemoAccordion() -> Element {
    rsx! {
        Accordion { allow_multiple_open: false, horizontal: false,
            for i in 0..3 {
                AccordionItem { index: i,
                    AccordionTrigger { "Section {i + 1}" }
                    AccordionContent {
                        div { class: "pb-4",
                            p { "Content for section {i + 1}. Lorem ipsum dolor sit amet." }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn DemoAlertDialog() -> Element {
    let mut open = use_signal(|| false);
    let mut confirmed = use_signal(|| false);
    rsx! {
        Button { variant: ButtonVariant::Outline, onclick: move |_| open.set(true), "Show Alert Dialog" }
        AlertDialogRoot { open: open(), on_open_change: move |v| open.set(v),
            AlertDialogContent {
                AlertDialogTitle { "Delete item" }
                AlertDialogDescription { "Are you sure? This action cannot be undone." }
                AlertDialogActions {
                    AlertDialogCancel { "Cancel" }
                    AlertDialogAction { on_click: move |_| confirmed.set(true), "Delete" }
                }
            }
        }
        if confirmed() {
            p { class: "mt-2 font-semibold text-danger", "Item deleted!" }
        }
    }
}

#[component]
fn DemoAspectRatio() -> Element {
    rsx! {
        div { class: "w-80",
            AspectRatio { ratio: 16.0 / 9.0,
                div { class: "w-full h-full flex items-center justify-center rounded-md bg-accent-subtle", "16:9" }
            }
        }
    }
}

#[component]
fn DemoAvatar() -> Element {
    rsx! {
        div { class: "flex gap-4 items-center",
            Avatar { size: AvatarImageSize::Small, aria_label: "User", AvatarFallback { "MC" } }
            Avatar { size: AvatarImageSize::Medium, aria_label: "User", AvatarFallback { "DX" } }
            Avatar { size: AvatarImageSize::Large, shape: AvatarShape::Rounded, aria_label: "User", AvatarFallback { "LG" } }
        }
    }
}

#[component]
fn DemoBadge() -> Element {
    rsx! {
        div { class: "flex gap-2 flex-wrap",
            Badge { "Primary" }
            Badge { variant: BadgeVariant::Secondary, "Secondary" }
            Badge { variant: BadgeVariant::Destructive, "Destructive" }
            Badge { variant: BadgeVariant::Outline, "Outline" }
        }
    }
}

#[component]
fn DemoCalendar() -> Element {
    let mut selected_date = use_signal(|| None::<Date>);
    let mut view_date = use_signal(|| UtcDateTime::now().date());
    rsx! {
        Calendar {
            selected_date: selected_date(),
            on_date_change: move |date| selected_date.set(date),
            view_date: view_date(),
            on_view_change: move |d: Date| view_date.set(d),
            CalendarView {
                CalendarHeader {
                    CalendarNavigation {
                        CalendarPreviousMonthButton {}
                        CalendarSelectMonth {}
                        CalendarSelectYear {}
                        CalendarNextMonthButton {}
                    }
                }
                CalendarGrid {}
            }
        }
        if let Some(d) = selected_date() {
            p { class: "mt-2 text-sm text-fg-muted", "Selected: {d}" }
        }
    }
}

#[component]
fn DemoCard() -> Element {
    rsx! {
        Card { class: "max-w-96",
            CardHeader {
                CardTitle { "Card Title" }
                CardDescription { "A description of the card content." }
            }
            CardContent { p { "This is the main content area of the card." } }
            CardFooter {
                Button { "Action" }
                Button { variant: ButtonVariant::Outline, "Cancel" }
            }
        }
    }
}

#[component]
fn DemoCheckbox() -> Element {
    rsx! {
        div { class: "flex flex-col gap-2",
            div { class: "flex items-center gap-2",
                Checkbox { name: "check1", aria_label: "Option 1" } span { "Option 1" }
            }
            div { class: "flex items-center gap-2",
                Checkbox { name: "check2", aria_label: "Option 2" } span { "Option 2" }
            }
            div { class: "flex items-center gap-2",
                Checkbox { name: "check3", aria_label: "Disabled", disabled: true }
                span { class: "opacity-50", "Disabled" }
            }
        }
    }
}

#[component]
fn DemoCollapsible() -> Element {
    rsx! {
        Collapsible {
            CollapsibleTrigger { b { "Recent Activity" } }
            CollapsibleList {
                CollapsibleItem { "Added new feature to accordion component" }
                CollapsibleContent {
                    CollapsibleItem { "Fixed bug in collapsible component" }
                    CollapsibleItem { "Updated documentation for toggle group" }
                }
            }
        }
    }
}

#[component]
fn DemoContextMenu() -> Element {
    let mut selected_item = use_signal(|| None::<String>);
    rsx! {
        ContextMenu {
            ContextMenuTrigger {
                div { class: "p-6 border border-dashed border-safe rounded-md text-center text-fg-muted", "Right-click here" }
            }
            ContextMenuContent {
                ContextMenuItem { value: "edit".to_string(), index: 0usize, on_select: move |v| selected_item.set(Some(v)), "Edit" }
                ContextMenuItem { value: "duplicate".to_string(), index: 1usize, on_select: move |v| selected_item.set(Some(v)), "Duplicate" }
                ContextMenuItem { value: "delete".to_string(), index: 2usize, on_select: move |v| selected_item.set(Some(v)), "Delete" }
            }
        }
        if let Some(item) = selected_item() {
            p { class: "mt-2 text-sm", "Selected: {item}" }
        }
    }
}

#[component]
fn DemoDatePicker() -> Element {
    let mut selected_date = use_signal(|| None::<Date>);
    rsx! {
        DatePicker {
            selected_date: selected_date(),
            on_value_change: move |v| selected_date.set(v),
            DatePickerInput {}
        }
        if let Some(d) = selected_date() {
            p { class: "mt-2 text-sm text-fg-muted", "Picked: {d}" }
        }
    }
}

#[component]
fn DemoDialog() -> Element {
    let mut open = use_signal(|| false);
    rsx! {
        Button { variant: ButtonVariant::Outline, onclick: move |_| open.set(true), "Open Dialog" }
        DialogRoot { open: open(), on_open_change: move |v| open.set(v),
            DialogContent {
                button { class: "dialog-close", r#type: "button", aria_label: "Close", onclick: move |_| open.set(false), "\u{00d7}" }
                DialogTitle { "Dialog Title" }
                DialogDescription { "This is a modal dialog with focus trapping." }
            }
        }
    }
}

#[component]
fn DemoDragAndDropList() -> Element {
    let items = vec![rsx!{"Item 1"}, rsx!{"Item 2"}, rsx!{"Item 3"}, rsx!{"Item 4"}, rsx!{"Item 5"}];
    rsx! { div { class: "max-w-64", DragAndDropList { items } } }
}

#[component]
fn DemoDropdownMenu() -> Element {
    let mut selected = use_signal(|| None::<String>);
    rsx! {
        DropdownMenu { default_open: false,
            DropdownMenuTrigger { "Open Menu" }
            DropdownMenuContent {
                DropdownMenuItem::<String> { value: "edit".to_string(), index: 0usize, on_select: move |v| selected.set(Some(v)), "Edit" }
                DropdownMenuItem::<String> { value: "duplicate".to_string(), index: 1usize, on_select: move |v| selected.set(Some(v)), "Duplicate" }
                DropdownMenuItem::<String> { value: "delete".to_string(), index: 2usize, on_select: move |v| selected.set(Some(v)), "Delete" }
            }
        }
        if let Some(op) = selected() { p { class: "mt-2 text-sm", "Selected: {op}" } }
    }
}

#[component]
fn DemoHoverCard() -> Element {
    rsx! {
        div { class: "p-10",
            HoverCard {
                HoverCardTrigger { i { class: "cursor-pointer underline", "Hover over me" } }
                HoverCardContent { side: ContentSide::Bottom,
                    div { class: "p-4",
                        h4 { class: "m-0 mb-2", "Information" }
                        p { class: "m-0", "This hover card shows additional context." }
                    }
                }
            }
        }
    }
}

#[component]
fn DemoInput() -> Element {
    let mut name = use_signal(String::new);
    rsx! {
        div { class: "flex flex-col gap-2 max-w-80",
            Label { html_for: "demo-input", "Name" }
            Input { id: "demo-input", placeholder: "Enter your name", value: name, oninput: move |e: FormEvent| name.set(e.value()) }
            if !name.read().is_empty() {
                p { class: "text-sm text-fg-secondary", "Hello, {name}!" }
            }
        }
    }
}

#[component]
fn DemoLabel() -> Element {
    rsx! {
        div { class: "flex flex-col gap-1",
            Label { html_for: "demo-label-input", "Email address" }
            Input { id: "demo-label-input", r#type: "email", placeholder: "you@example.com" }
        }
    }
}

#[component]
fn DemoMenubar() -> Element {
    rsx! {
        dx_menubar::Menubar {
            dx_menubar::MenubarMenu { index: 0usize,
                dx_menubar::MenubarTrigger { "File" }
                dx_menubar::MenubarContent {
                    dx_menubar::MenubarItem { index: 0usize, value: "new".to_string(), on_select: move |_| {}, "New" }
                    dx_menubar::MenubarItem { index: 1usize, value: "open".to_string(), on_select: move |_| {}, "Open" }
                    dx_menubar::MenubarItem { index: 2usize, value: "save".to_string(), on_select: move |_| {}, "Save" }
                }
            }
            dx_menubar::MenubarMenu { index: 1usize,
                dx_menubar::MenubarTrigger { "Edit" }
                dx_menubar::MenubarContent {
                    dx_menubar::MenubarItem { index: 0usize, value: "cut".to_string(), on_select: move |_| {}, "Cut" }
                    dx_menubar::MenubarItem { index: 1usize, value: "copy".to_string(), on_select: move |_| {}, "Copy" }
                    dx_menubar::MenubarItem { index: 2usize, value: "paste".to_string(), on_select: move |_| {}, "Paste" }
                }
            }
        }
    }
}

#[component]
fn DemoPagination() -> Element {
    rsx! {
        Pagination {
            PaginationContent {
                PaginationItem { PaginationPrevious { href: "#" } }
                PaginationItem { PaginationLink { href: "#", "1" } }
                PaginationItem { PaginationLink { href: "#", is_active: true, "2" } }
                PaginationItem { PaginationLink { href: "#", "3" } }
                PaginationItem { PaginationEllipsis {} }
                PaginationItem { PaginationNext { href: "#" } }
            }
        }
    }
}

#[component]
fn DemoPopover() -> Element {
    let mut open = use_signal(|| false);
    rsx! {
        PopoverRoot { open: open(), on_open_change: move |v| open.set(v),
            PopoverTrigger { "Show Popover" }
            PopoverContent { gap: "0.25rem",
                h3 { class: "m-0 py-1 text-center", "Popover Content" }
                p { class: "m-0 text-sm text-fg-muted", "This is a popover with arbitrary content." }
                Button { variant: ButtonVariant::Outline, onclick: move |_| open.set(false), "Close" }
            }
        }
    }
}

#[component]
fn DemoProgress() -> Element {
    let mut progress = use_signal(|| 45.0_f64);
    rsx! {
        div { class: "flex flex-col gap-2 max-w-80",
            p { class: "text-sm text-fg-muted", "{progress:.0}%" }
            Progress { aria_label: "Progress demo", value: progress(), ProgressIndicator {} }
            div { class: "flex gap-2",
                Button { variant: ButtonVariant::Outline, onclick: move |_| progress.set((progress() - 10.0).max(0.0)), "-10" }
                Button { variant: ButtonVariant::Outline, onclick: move |_| progress.set((progress() + 10.0).min(100.0)), "+10" }
            }
        }
    }
}

#[component]
fn DemoRadioGroup() -> Element {
    rsx! {
        RadioGroup {
            RadioItem { value: "option1".to_string(), index: 0usize, "Blue" }
            RadioItem { value: "option2".to_string(), index: 1usize, "Red" }
            RadioItem { value: "option3".to_string(), index: 2usize, disabled: true, "Green (disabled)" }
        }
    }
}

#[component]
fn DemoScrollArea() -> Element {
    rsx! {
        ScrollArea {
            width: "12em", height: "10em",
            border: "1px solid rgba(128,128,128,0.4)", border_radius: "0.5em", padding: "0 1em 1em 1em",
            direction: ScrollDirection::Vertical,
            div { for i in 1..=20 { p { "Scrollable item {i}" } } }
        }
    }
}

#[component]
fn DemoSelect() -> Element {
    rsx! {
        Select::<Option<String>> { placeholder: "Select a fruit...",
            SelectTrigger { aria_label: "Fruit select", width: "12rem", SelectValue {} }
            SelectList { aria_label: "Fruits",
                SelectGroup {
                    SelectGroupLabel { "Fruits" }
                    SelectOption::<Option<String>> { index: 0usize, value: "apple".to_string(), text_value: "Apple", "Apple" SelectItemIndicator {} }
                    SelectOption::<Option<String>> { index: 1usize, value: "banana".to_string(), text_value: "Banana", "Banana" SelectItemIndicator {} }
                    SelectOption::<Option<String>> { index: 2usize, value: "orange".to_string(), text_value: "Orange", "Orange" SelectItemIndicator {} }
                }
            }
        }
    }
}

#[component]
fn DemoSeparator() -> Element {
    rsx! {
        div { class: "max-w-80",
            "One thing"
            Separator { class: "my-3 w-full", horizontal: true, decorative: true }
            "Another thing"
            Separator { class: "my-3 w-full", horizontal: true, decorative: true }
            "A third thing"
        }
    }
}

#[component]
fn DemoSheet() -> Element {
    let mut open = use_signal(|| false);
    let mut side = use_signal(|| SheetSide::Right);
    let open_sheet = move |s: SheetSide| move |_| { side.set(s); open.set(true); };
    rsx! {
        div { class: "flex gap-2",
            Button { variant: ButtonVariant::Outline, onclick: open_sheet(SheetSide::Top), "Top" }
            Button { variant: ButtonVariant::Outline, onclick: open_sheet(SheetSide::Right), "Right" }
            Button { variant: ButtonVariant::Outline, onclick: open_sheet(SheetSide::Bottom), "Bottom" }
            Button { variant: ButtonVariant::Outline, onclick: open_sheet(SheetSide::Left), "Left" }
        }
        Sheet { open: open(), on_open_change: move |v| open.set(v),
            SheetContent { side: side(),
                SheetHeader {
                    SheetTitle { "Sheet Title" }
                    SheetDescription { "This is a sheet panel that slides in from the edge." }
                }
                div { class: "p-4 flex-1",
                    p { "Sheet body content goes here." }
                    div { class: "flex flex-col gap-2 max-w-64",
                        Label { html_for: "sheet-name", "Name" }
                        Input { id: "sheet-name", placeholder: "Enter name" }
                    }
                }
                SheetFooter {
                    Button { "Save" }
                    Button { variant: ButtonVariant::Outline, onclick: move |_| open.set(false), "Cancel" }
                }
            }
        }
    }
}

#[component]
fn DemoSkeleton() -> Element {
    rsx! {
        div { class: "flex flex-col gap-2 max-w-80",
            Skeleton { class: "h-8 w-3/5" }
            Skeleton { class: "h-4 w-full" }
            Skeleton { class: "h-4 w-4/5" }
            Skeleton { class: "h-32 w-full rounded-md" }
        }
    }
}

#[component]
fn DemoSlider() -> Element {
    let mut value = use_signal(|| 50.0);
    rsx! {
        div { class: "flex flex-col gap-2 max-w-80",
            p { class: "text-sm font-bold", "{value:.0}%" }
            Slider {
                label: "Demo Slider", horizontal: true, min: 0.0, max: 100.0, step: 1.0,
                default_value: SliderValue::Single(50.0),
                on_value_change: move |v: SliderValue| { let SliderValue::Single(val) = v; value.set(val); },
                SliderTrack { SliderRange {} SliderThumb {} }
            }
        }
    }
}

#[component]
fn DemoSwitch() -> Element {
    let mut checked = use_signal(|| false);
    rsx! {
        div { class: "flex items-center gap-2",
            Switch { checked: checked(), aria_label: "Toggle", on_checked_change: move |v| checked.set(v), SwitchThumb {} }
            span { class: "text-sm", if checked() { "On" } else { "Off" } }
        }
    }
}

#[component]
fn DemoTabs() -> Element {
    rsx! {
        Tabs { default_value: "tab1".to_string(), horizontal: true,
            TabList {
                TabTrigger { value: "tab1".to_string(), index: 0usize, "Account" }
                TabTrigger { value: "tab2".to_string(), index: 1usize, "Settings" }
                TabTrigger { value: "tab3".to_string(), index: 2usize, "Billing" }
            }
            TabContent { index: 0usize, value: "tab1".to_string(), div { class: "p-4", "Account settings and profile information." } }
            TabContent { index: 1usize, value: "tab2".to_string(), div { class: "p-4", "Application preferences and configuration." } }
            TabContent { index: 2usize, value: "tab3".to_string(), div { class: "p-4", "Billing history and payment methods." } }
        }
    }
}

#[component]
fn DemoTextarea() -> Element {
    let mut text = use_signal(String::new);
    rsx! {
        div { class: "flex flex-col gap-2 max-w-80",
            Label { html_for: "demo-textarea", "Description" }
            Textarea { id: "demo-textarea", placeholder: "Enter a description...", value: text, oninput: move |e: FormEvent| text.set(e.value()) }
            if !text.read().is_empty() {
                p { class: "text-sm text-fg-muted", "{text.read().len()} characters" }
            }
        }
    }
}

#[component]
fn DemoToast() -> Element {
    rsx! {
        p { class: "text-sm text-fg-muted",
            "Toast notifications appear as transient messages. Compose with ToastRoot, ToastTitle, ToastDescription, ToastAction, ToastClose."
        }
        div { class: "mt-2 p-3 border border-safe rounded-md max-w-80",
            div { class: "font-semibold mb-1", "Example Toast" }
            div { class: "text-sm text-fg-muted", "Your changes have been saved." }
        }
    }
}

#[component]
fn DemoToggle() -> Element {
    rsx! {
        div { class: "flex gap-2",
            Toggle { width: "2rem", height: "2rem", em { "B" } }
            Toggle { width: "2rem", height: "2rem", em { "I" } }
            Toggle { width: "2rem", height: "2rem", em { "U" } }
        }
    }
}

#[component]
fn DemoToggleGroup() -> Element {
    rsx! {
        ToggleGroup { horizontal: true, allow_multiple_pressed: true,
            ToggleItem { index: 0usize, b { "B" } }
            ToggleItem { index: 1usize, i { "I" } }
            ToggleItem { index: 2usize, u { "U" } }
        }
    }
}

#[component]
fn DemoToolbar() -> Element {
    let mut is_bold = use_signal(|| false);
    let mut is_italic = use_signal(|| false);
    rsx! {
        Toolbar { aria_label: "Text formatting",
            ToolbarGroup {
                ToolbarButton { index: 0usize, on_click: move |_| is_bold.toggle(), "data-state": if is_bold() { "on" } else { "off" }, "Bold" }
                ToolbarButton { index: 1usize, on_click: move |_| is_italic.toggle(), "data-state": if is_italic() { "on" } else { "off" }, "Italic" }
            }
        }
        p {
            class: "mt-2",
            font_weight: if is_bold() { "bold" } else { "normal" },
            font_style: if is_italic() { "italic" } else { "normal" },
            "Sample text — click toolbar buttons to format."
        }
    }
}

#[component]
fn DemoTooltip() -> Element {
    rsx! {
        Tooltip {
            TooltipTrigger { Button { variant: ButtonVariant::Outline, "Hover me" } }
            TooltipContent { side: ContentSide::Bottom, p { class: "m-0", "This is a tooltip with rich content." } }
        }
    }
}

#[component]
fn DemoVirtualList() -> Element {
    let count = use_signal(|| 10_000usize);
    rsx! {
        div { class: "h-72 w-80 border border-safe rounded-md overflow-hidden",
            VirtualList {
                count: count(),
                estimate_size: move |_idx: usize| 32u32,
                render_item: move |idx: usize| rsx! {
                    div { class: "px-3 py-1 text-sm border-b-safe h-8", "Item {idx + 1} of 10,000" }
                },
            }
        }
    }
}

// ── Custom cosmix component demos ──

#[component]
fn DemoDataTable() -> Element {
    let rows = vec![
        serde_json::json!({"name": "main.rs", "size": 12480, "modified": "2026-03-31"}),
        serde_json::json!({"name": "lib.rs", "size": 3200, "modified": "2026-03-30"}),
        serde_json::json!({"name": "theme.rs", "size": 4800, "modified": "2026-03-29"}),
        serde_json::json!({"name": "mod.rs", "size": 580, "modified": "2026-03-28"}),
        serde_json::json!({"name": "icons.rs", "size": 6100, "modified": "2026-03-27"}),
    ];
    let mut selected = use_signal(|| None::<usize>);
    rsx! {
        div { class: "max-w-xl",
            DataTable {
                columns: vec![
                    DataColumn { key: "name", label: "Name", width: "1fr", sortable: true, format: None },
                    DataColumn { key: "size", label: "Size", width: "80px", sortable: true, format: Some(fmt_size) },
                    DataColumn { key: "modified", label: "Modified", width: "100px", sortable: true, format: None },
                ],
                rows, on_row_click: move |idx| selected.set(Some(idx)), selected: selected(),
            }
        }
    }
}

fn fmt_size(v: &serde_json::Value) -> String {
    match v.as_f64() {
        Some(n) if n >= 1024.0 => format!("{:.1}K", n / 1024.0),
        Some(n) => format!("{n:.0}"),
        None => String::new(),
    }
}

// ── Sidebar demo (full upstream example) ──

#[derive(Clone, PartialEq)]
struct Team { name: &'static str, plan: &'static str }

#[derive(Clone, PartialEq)]
struct NavMainItem { title: &'static str, url: &'static str, is_active: bool, items: &'static [SubItem] }

#[derive(Clone, PartialEq)]
struct SubItem { title: &'static str, url: &'static str }

#[derive(Clone, PartialEq)]
struct Project { name: &'static str, url: &'static str }

const TEAMS: &[Team] = &[
    Team { name: "Acme Inc", plan: "Enterprise" },
    Team { name: "Acme Corp.", plan: "Startup" },
    Team { name: "Evil Corp.", plan: "Free" },
];

const NAV_MAIN: &[NavMainItem] = &[
    NavMainItem { title: "Playground", url: "#", is_active: true, items: &[
        SubItem { title: "History", url: "#" }, SubItem { title: "Starred", url: "#" }, SubItem { title: "Settings", url: "#" },
    ]},
    NavMainItem { title: "Models", url: "#", is_active: false, items: &[
        SubItem { title: "Genesis", url: "#" }, SubItem { title: "Explorer", url: "#" }, SubItem { title: "Quantum", url: "#" },
    ]},
    NavMainItem { title: "Documentation", url: "#", is_active: false, items: &[
        SubItem { title: "Introduction", url: "#" }, SubItem { title: "Get Started", url: "#" }, SubItem { title: "Tutorials", url: "#" }, SubItem { title: "Changelog", url: "#" },
    ]},
    NavMainItem { title: "Settings", url: "#", is_active: false, items: &[
        SubItem { title: "General", url: "#" }, SubItem { title: "Team", url: "#" }, SubItem { title: "Billing", url: "#" }, SubItem { title: "Limits", url: "#" },
    ]},
];

const PROJECTS: &[Project] = &[
    Project { name: "Design Engineering", url: "#" },
    Project { name: "Sales & Marketing", url: "#" },
    Project { name: "Travel", url: "#" },
];

#[component]
fn DemoSidebar() -> Element {
    let side = use_signal(|| SidebarSide::Left);
    let collapsible = use_signal(|| SidebarCollapsible::Offcanvas);
    rsx! {
        document::Stylesheet { href: asset!("/assets/sidebar-demo.css") }
        SidebarProvider {
            Sidebar {
                variant: SidebarVariant::Sidebar, collapsible: collapsible(), side: side(),
                SidebarHeader { SbTeamSwitcher { teams: TEAMS } }
                SidebarContent {
                    SbNavMain { items: NAV_MAIN }
                    SbNavProjects { projects: PROJECTS }
                }
                SidebarFooter { SbNavUser {} }
                SidebarRail {}
            }
            SidebarInset {
                header { class: "flex items-center justify-between h-14 shrink-0 px-4 border-b-safe",
                    div { class: "flex items-center gap-3",
                        SidebarTrigger {}
                        Separator { height: "1rem", horizontal: false }
                        span { "Sidebar Demo" }
                    }
                }
                div { class: "flex flex-1 flex-col gap-6 p-6 min-h-0 overflow-y-auto overflow-x-hidden",
                    SbSettingControls { side, collapsible }
                    Skeleton { class: "h-40 w-full shrink-0" }
                    Skeleton { class: "h-80 w-full shrink-0" }
                }
            }
        }
    }
}

#[component]
fn SbTeamSwitcher(teams: &'static [Team]) -> Element {
    let mut active_team = use_signal(|| 0usize);
    rsx! {
        SidebarMenu {
            SidebarMenuItem {
                DropdownMenu {
                    DropdownMenuTrigger {
                        as: move |attributes: Vec<Attribute>| rsx! {
                            SidebarMenuButton { size: SidebarMenuButtonSize::Lg, attributes,
                                div { class: "flex shrink-0 items-center justify-center w-8 h-8 rounded-lg",
                                    style: "background:var(--sidebar-accent);color:var(--sidebar-accent-foreground);",
                                    SbIcon {}
                                }
                                div { class: "sidebar-info-block",
                                    span { class: "sidebar-info-title", {teams[active_team()].name} }
                                    span { class: "sidebar-info-subtitle", {teams[active_team()].plan} }
                                }
                                SbChevronIcon {}
                            }
                        },
                    }
                    DropdownMenuContent {
                        div { class: "p-2 text-xs opacity-70", "Teams" }
                        for (idx, team) in teams.iter().enumerate() {
                            DropdownMenuItem {
                                index: idx, value: idx, on_select: move |v: usize| active_team.set(v),
                                SbIcon {} {team.name}
                                span { class: "ml-auto text-xs opacity-70", "\u{2318}{idx + 1}" }
                            }
                        }
                        Separator { decorative: true }
                        DropdownMenuItem {
                            index: teams.len(), value: 999usize, on_select: move |_: usize| {},
                            SbIcon {} div { class: "opacity-70 font-medium", "Add team" }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn SbNavMain(items: &'static [NavMainItem]) -> Element {
    rsx! {
        SidebarGroup {
            SidebarGroupLabel { "Platform" }
            SidebarMenu {
                for item in items.iter() {
                    Collapsible {
                        default_open: item.is_active,
                        as: move |attributes: Vec<Attribute>| rsx! {
                            SidebarMenuItem { key: "{item.title}", attributes,
                                CollapsibleTrigger {
                                    as: move |attributes: Vec<Attribute>| rsx! {
                                        SidebarMenuButton {
                                            tooltip: rsx! { {item.title} }, attributes,
                                            SbIcon {} span { {item.title} } SbChevronIcon {}
                                        }
                                    },
                                }
                                CollapsibleContent {
                                    SidebarMenuSub {
                                        for sub_item in item.items {
                                            SidebarMenuSubItem { key: "{sub_item.title}",
                                                SidebarMenuSubButton {
                                                    as: move |attributes: Vec<Attribute>| rsx! {
                                                        a { href: sub_item.url, ..attributes, span { {sub_item.title} } }
                                                    },
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        },
                    }
                }
            }
        }
    }
}

#[component]
fn SbNavProjects(projects: &'static [Project]) -> Element {
    rsx! {
        SidebarGroup { class: "sidebar-hide-on-collapse",
            SidebarGroupLabel { "Projects" }
            SidebarMenu {
                for project in projects.iter() {
                    SidebarMenuItem { key: "{project.name}",
                        SidebarMenuButton {
                            as: move |attributes: Vec<Attribute>| rsx! {
                                a { href: project.url, ..attributes, SbIcon {} span { {project.name} } }
                            },
                        }
                        DropdownMenu {
                            DropdownMenuTrigger {
                                as: move |attributes: Vec<Attribute>| rsx! {
                                    SidebarMenuAction { show_on_hover: true, attributes, SbIcon {} span { class: "sr-only", "More" } }
                                },
                            }
                            DropdownMenuContent {
                                DropdownMenuItem { index: 0usize, value: "view".to_string(), on_select: move |_: String| {}, SbIcon {} span { "View Project" } }
                                DropdownMenuItem { index: 1usize, value: "share".to_string(), on_select: move |_: String| {}, SbIcon {} span { "Share Project" } }
                                Separator { decorative: true }
                                DropdownMenuItem { index: 2usize, value: "delete".to_string(), on_select: move |_: String| {}, SbIcon {} span { "Delete Project" } }
                            }
                        }
                    }
                }
                SidebarMenuItem {
                    SidebarMenuButton { class: "opacity-70 font-medium", SbIcon {} span { "More" } }
                    SidebarMenuBadge { "+99" }
                }
            }
        }
    }
}

#[component]
fn SbNavUser() -> Element {
    rsx! {
        SidebarMenu {
            SidebarMenuItem {
                DropdownMenu {
                    DropdownMenuTrigger {
                        as: move |attributes: Vec<Attribute>| rsx! {
                            SidebarMenuButton { size: SidebarMenuButtonSize::Lg, attributes,
                                Avatar { size: AvatarImageSize::Small, style: "border-radius:0.5rem;", AvatarFallback { "MC" } }
                                div { class: "sidebar-info-block",
                                    span { class: "sidebar-info-title", "Mark Constable" }
                                    span { class: "sidebar-info-subtitle", "mc@cosmix.dev" }
                                }
                                SbChevronIcon {}
                            }
                        },
                    }
                    DropdownMenuContent {
                        div { class: "flex items-center gap-2 p-1 text-left text-sm",
                            Avatar { size: AvatarImageSize::Small, style: "border-radius:0.5rem;", AvatarFallback { "MC" } }
                            div { class: "sidebar-info-block",
                                span { class: "sidebar-info-title", "Mark Constable" }
                                span { class: "sidebar-info-subtitle", "mc@cosmix.dev" }
                            }
                        }
                        Separator { decorative: true }
                        DropdownMenuItem { index: 0usize, value: "account".to_string(), on_select: move |_: String| {}, SbIcon {} "Account" }
                        DropdownMenuItem { index: 1usize, value: "billing".to_string(), on_select: move |_: String| {}, SbIcon {} "Billing" }
                        DropdownMenuItem { index: 2usize, value: "notifications".to_string(), on_select: move |_: String| {}, SbIcon {} "Notifications" }
                        Separator { decorative: true }
                        DropdownMenuItem { index: 3usize, value: "logout".to_string(), on_select: move |_: String| {}, SbIcon {} "Log out" }
                    }
                }
            }
        }
    }
}

#[component]
fn SbSettingControls(side: Signal<SidebarSide>, collapsible: Signal<SidebarCollapsible>) -> Element {
    rsx! {
        div { class: "flex flex-col gap-3 p-3 border border-safe rounded-xl",
            div { class: "flex items-center justify-between gap-3 flex-wrap",
                span { class: "text-xs font-semibold", "Side" }
                div { class: "inline-flex gap-2",
                    Button { variant: if side() == SidebarSide::Left { ButtonVariant::Primary } else { ButtonVariant::Outline }, onclick: move |_| side.set(SidebarSide::Left), class: "text-xs px-2 py-1", "Left" }
                    Button { variant: if side() == SidebarSide::Right { ButtonVariant::Primary } else { ButtonVariant::Outline }, onclick: move |_| side.set(SidebarSide::Right), class: "text-xs px-2 py-1", "Right" }
                }
            }
            div { class: "flex items-center justify-between gap-3 flex-wrap",
                span { class: "text-xs font-semibold", "Collapse" }
                div { class: "inline-flex gap-2 flex-wrap",
                    Button { variant: if collapsible() == SidebarCollapsible::Offcanvas { ButtonVariant::Primary } else { ButtonVariant::Outline }, onclick: move |_| collapsible.set(SidebarCollapsible::Offcanvas), class: "text-xs px-2 py-1", "Offcanvas" }
                    Button { variant: if collapsible() == SidebarCollapsible::Icon { ButtonVariant::Primary } else { ButtonVariant::Outline }, onclick: move |_| collapsible.set(SidebarCollapsible::Icon), class: "text-xs px-2 py-1", "Icon" }
                    Button { variant: if collapsible() == SidebarCollapsible::None { ButtonVariant::Primary } else { ButtonVariant::Outline }, onclick: move |_| collapsible.set(SidebarCollapsible::None), class: "text-xs px-2 py-1", "None" }
                }
            }
        }
    }
}

#[component]
fn SbIcon(#[props(default = "sidebar-icon")] class: &'static str) -> Element {
    rsx! {
        dioxus_primitives::icon::Icon { class, width: "24px", height: "24px",
            circle { cx: "12", cy: "12", r: "10" }
        }
    }
}

#[component]
fn SbChevronIcon() -> Element {
    rsx! {
        dioxus_primitives::icon::Icon { class: "sidebar-icon sidebar-chevron", width: "24px", height: "24px",
            path { d: "m9 18 6-6-6-6" }
        }
    }
}
