; Alloy 6 (the modeling language), vincentlu/tree-sitter-alloy grammar.

; Comments
(line_comment) @comment.line
(block_comment) @comment.block

; Literals
(string) @string
(number) @constant.numeric.integer

; Builtin atoms
(this_expr) @variable.builtin
(iden_expr) @constant.builtin
(none_expr) @constant.builtin
(univ_expr) @constant.builtin
(int_expr) @type.builtin

[
  "Int"
  "String"
  "steps"
] @type.builtin

; Declarations
(sig_decl names: (name_list (name) @type))
(enum_decl name: (name) @type)
(enum_decl (name_list (name) @constant))

(sig_extension (sig_ref (qual_name (name) @type)))
(sig_extension (sig_ref_union (sig_ref (qual_name (name) @type))))

(pred_decl name: (name) @function)
(fun_decl name: (name) @function)
(let_decl name: (name) @function.macro)
(pred_decl (sig_ref (qual_name (name) @type)))
(fun_decl (sig_ref (qual_name (name) @type)))

(fact_decl name: (name) @label)
(assert_decl name: (name) @label)
(command label: (name) @label)
(command name: (qual_name) @function)

; Modules
(module_decl name: (qual_name) @namespace)
(open_decl name: (qual_name) @namespace)
(open_decl alias: (name) @namespace)

; Variables
(field_decl (name_list (name) @variable.other.member))
(decl (name_list (name) @variable.parameter))
(let_binding (name) @variable)
(at_name (name) @variable)

; Keywords
[
  "module"
  "open"
  "as"
] @keyword.control.import

[
  "sig"
  "enum"
  "fact"
  "pred"
  "fun"
  "assert"
  "let"
] @keyword.storage.type

[
  "abstract"
  "private"
  "var"
  "extends"
  "in"
  "disj"
] @keyword.storage.modifier

[
  "run"
  "check"
  "for"
  "but"
  "expect"
  "exactly"
] @keyword.directive

[
  "all"
  "no"
  "some"
  "lone"
  "one"
  "sum"
  "set"
  "seq"
] @keyword

(some_arrow_op) @keyword
(one_arrow_op) @keyword
(lone_arrow_op) @keyword

[
  "not"
  "and"
  "or"
  "implies"
  "iff"
  "else"
  "always"
  "eventually"
  "after"
  "before"
  "historically"
  "once"
  "until"
  "since"
  "releases"
  "triggered"
] @keyword.operator

; Operators
[
  "="
  "!="
  "!"
  "!in"
  "!<"
  "!>"
  "!<="
  "!>="
  "=<"
  "<"
  ">"
  "<="
  ">="
  "<=>"
  "=>"
  "&&"
  "||"
  "+"
  "-"
  "++"
  "&"
  "->"
  "<:"
  ":>"
  "."
  "~"
  "^"
  "*"
  "#"
  "<<"
  ">>"
  ">>>"
  ";"
  "'"
] @operator

; Punctuation
[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

[
  ","
  ":"
  "|"
  "/"
  ".."
] @punctuation.delimiter
