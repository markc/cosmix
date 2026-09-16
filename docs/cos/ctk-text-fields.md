# CTK text fields

`CtkTextFieldProps::placeholder(text)` displays ghost text while a plain
single-line field is empty. The hint is a separate, non-interactive text
entity: it never becomes the field value, selection, clipboard content or
submitted text. An active IME composition hides the hint until composition
ends. Updating a field does not require replacing its editable entity.

`CtkTextFieldPlaceholder` identifies the hint and its input entity for hosts
that need to update the hint after construction. Its `Text` component holds
the displayed placeholder.
