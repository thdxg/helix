; inherits: html

; The pinned tree-sitter-blade has no node for `{{ }}` brackets, so only
; directive pairs (`@if` … `@endif`, `@section` … `@endsection`, …) fold.
((directive_start) @start
    (directive_end) @end.after
    (#set! role block))
