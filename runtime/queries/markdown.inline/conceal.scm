; Markup hidden by `editor.conceal`, styled after glow's rendering.

; Emphasis (`*`, `_`), strong emphasis and strikethrough delimiters.
(emphasis_delimiter) @conceal

; Code spans: the backticks.
(code_span_delimiter) @conceal

; Links show as the link text followed by the destination, like glow.
(inline_link "[" @conceal)
(inline_link "]" @conceal (#set! conceal " "))
(inline_link ["(" ")"] @conceal)
(inline_link (link_title) @conceal)
(shortcut_link ["[" "]"] @conceal)
(collapsed_reference_link ["[" "]"] @conceal)
(full_reference_link ["[" "]"] @conceal)
(full_reference_link (link_label) @conceal)

; Images show as the description followed by the source.
(image "!" @conceal)
(image "[" @conceal)
(image "]" @conceal (#set! conceal " → "))
(image ["(" ")"] @conceal)
(image (link_title) @conceal)
