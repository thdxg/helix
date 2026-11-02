; Markup hidden by `editor.conceal`, styled after glow's rendering.

; Headings: hide the marker and the space after it; the heading text keeps its highlight.
((atx_h1_marker) @conceal (#set! conceal.trailing-whitespace))
((atx_h2_marker) @conceal (#set! conceal.trailing-whitespace))
((atx_h3_marker) @conceal (#set! conceal.trailing-whitespace))
((atx_h4_marker) @conceal (#set! conceal.trailing-whitespace))
((atx_h5_marker) @conceal (#set! conceal.trailing-whitespace))
((atx_h6_marker) @conceal (#set! conceal.trailing-whitespace))

; Bullets. The marker tokens include the whitespace after them, which stays visible.
((list_marker_minus) @conceal (#set! conceal "•") (#set! conceal.trim-whitespace))
((list_marker_plus) @conceal (#set! conceal "•") (#set! conceal.trim-whitespace))
((list_marker_star) @conceal (#set! conceal "•") (#set! conceal.trim-whitespace))

; Task lists.
((task_list_marker_checked) @conceal (#set! conceal "[✓]"))

; Block quotes.
((block_quote_marker) @conceal (#set! conceal "│") (#set! conceal.trim-whitespace))
((block_continuation) @conceal
  (#match? @conceal "^>")
  (#set! conceal "│")
  (#set! conceal.trim-whitespace))

; Fenced code blocks: the fences and the language tag.
(fenced_code_block_delimiter) @conceal
(info_string) @conceal

; Tables.
(pipe_table_header "|" @conceal (#set! conceal "│"))
(pipe_table_row "|" @conceal (#set! conceal "│"))
