//! Startup highlight definitions, verbatim from upstream
//! `highlight_group.c` (`highlight_init_both` / `_light` / `_dark`),
//! fed through the `:highlight` path at editor construction the way
//! `init_highlight` (highlight_group.c:755-800) seeds `hl_table` in
//! `main()`. Attribute strings keep upstream spelling; the command
//! parser lower-cases keys.

/// Groups defined regardless of `background` (the `both` list).
pub const HIGHLIGHT_INIT_BOTH: &[&str] = &[
    "Cursor            guifg=bg      guibg=fg",
    "CursorLineNr      gui=bold      cterm=bold",
    "Ignore            guifg=bg      ctermfg=0",
    "PmenuMatch        gui=bold      cterm=bold",
    "PmenuMatchSel     gui=bold      cterm=bold",
    "PmenuSel          gui=reverse   cterm=reverse,underline blend=0",
    "RedrawDebugNormal gui=reverse   cterm=reverse",
    "TabLineSel        guifg=fg      guibg=bg                gui=bold cterm=nocombine",
    "TermCursor        gui=reverse   cterm=reverse",
    "Underlined        gui=underline cterm=underline",
    "lCursor           guifg=bg      guibg=fg",
    "default link CursorIM         Cursor",
    "default link CursorLineFold   FoldColumn",
    "default link CursorLineSign   SignColumn",
    "default link DiffTextAdd      DiffText",
    "default link EndOfBuffer      NonText",
    "default link FloatBorder      NormalFloat",
    "default link FloatFooter      FloatTitle",
    "default link FloatTitle       Title",
    "default link FoldColumn       SignColumn",
    "default link IncSearch        CurSearch",
    "default link LineNrAbove      LineNr",
    "default link LineNrBelow      LineNr",
    "default link MsgSeparator     StatusLine",
    "default link MsgArea          NONE",
    "default link NormalNC         NONE",
    "default link PmenuExtra       Pmenu",
    "default link PmenuExtraSel    PmenuSel",
    "default link PmenuKind        Pmenu",
    "default link PmenuKindSel     PmenuSel",
    "default link PmenuSbar        Pmenu",
    "default link PmenuBorder        Pmenu",
    "default link PmenuShadow        FloatShadow",
    "default link PmenuShadowThrough FloatShadowThrough",
    "default link PreInsert        Added",
    "default link ComplMatchIns    NONE",
    "default link ComplHint        NonText",
    "default link ComplHintMore    MoreMsg",
    "default link Substitute       Search",
    "default link StatusLineTerm   StatusLine",
    "default link StatusLineTermNC StatusLineNC",
    "default link StderrMsg        ErrorMsg",
    "default link StdoutMsg        NONE",
    "default link TabLine          StatusLineNC",
    "default link TabLineFill      TabLine",
    "default link VertSplit        WinSeparator",
    "default link VisualNOS        Visual",
    "default link Whitespace       NonText",
    "default link WildMenu         PmenuSel",
    "default link WinSeparator     Normal",
    "default link Character      Constant",
    "default link Number         Constant",
    "default link Boolean        Constant",
    "default link Float          Number",
    "default link Conditional    Statement",
    "default link Repeat         Statement",
    "default link Label          Statement",
    "default link Keyword        Statement",
    "default link Exception      Statement",
    "default link Include        PreProc",
    "default link Define         PreProc",
    "default link Macro          PreProc",
    "default link PreCondit      PreProc",
    "default link StorageClass   Type",
    "default link Structure      Type",
    "default link Typedef        Type",
    "default link Tag            Special",
    "default link SpecialChar    Special",
    "default link SpecialComment Special",
    "default link Debug          Special",
    "default link SpecialKey     Special",
    "default link Dimmed         Comment",
    "default link LspCodeLens                 NonText",
    "default link LspCodeLensSeparator        LspCodeLens",
    "default link LspInlayHint                NonText",
    "default link LspReferenceRead            LspReferenceText",
    "default link LspReferenceText            Visual",
    "default link LspReferenceWrite           LspReferenceText",
    "default link LspReferenceTarget          LspReferenceText",
    "default link LspSignatureActiveParameter Visual",
    "default link SnippetTabstop              Visual",
    "default link SnippetTabstopActive        SnippetTabstop",
    "default link DiagnosticFloatingError    DiagnosticError",
    "default link DiagnosticFloatingWarn     DiagnosticWarn",
    "default link DiagnosticFloatingInfo     DiagnosticInfo",
    "default link DiagnosticFloatingHint     DiagnosticHint",
    "default link DiagnosticFloatingOk       DiagnosticOk",
    "default link DiagnosticVirtualTextError DiagnosticError",
    "default link DiagnosticVirtualTextWarn  DiagnosticWarn",
    "default link DiagnosticVirtualTextInfo  DiagnosticInfo",
    "default link DiagnosticVirtualTextHint  DiagnosticHint",
    "default link DiagnosticVirtualTextOk    DiagnosticOk",
    "default link DiagnosticVirtualLinesError DiagnosticError",
    "default link DiagnosticVirtualLinesWarn  DiagnosticWarn",
    "default link DiagnosticVirtualLinesInfo  DiagnosticInfo",
    "default link DiagnosticVirtualLinesHint  DiagnosticHint",
    "default link DiagnosticVirtualLinesOk    DiagnosticOk",
    "default link DiagnosticSignError        DiagnosticError",
    "default link DiagnosticSignWarn         DiagnosticWarn",
    "default link DiagnosticSignInfo         DiagnosticInfo",
    "default link DiagnosticSignHint         DiagnosticHint",
    "default link DiagnosticSignOk           DiagnosticOk",
    "default link DiagnosticUnnecessary      Comment",
    "default link @variable.builtin           Special",
    "default link @variable.parameter.builtin Special",
    "default link @constant         Constant",
    "default link @constant.builtin Special",
    "default link @module         Structure",
    "default link @module.builtin Special",
    "default link @label          Label",
    "default link @string             String",
    "default link @string.regexp      @string.special",
    "default link @string.escape      @string.special",
    "default link @string.special     SpecialChar",
    "default link @string.special.url Underlined",
    "default link @character         Character",
    "default link @character.special SpecialChar",
    "default link @boolean      Boolean",
    "default link @number       Number",
    "default link @number.float Float",
    "default link @type         Type",
    "default link @type.builtin Special",
    "default link @attribute         Macro",
    "default link @attribute.builtin Special",
    "default link @property          Identifier",
    "default link @function         Function",
    "default link @function.builtin Special",
    "default link @constructor Special",
    "default link @operator    Operator",
    "default link @keyword Keyword",
    "default link @punctuation         Delimiter",
    "default link @punctuation.special Special",
    "default link @comment Comment",
    "default link @comment.error   DiagnosticError",
    "default link @comment.warning DiagnosticWarn",
    "default link @comment.note    DiagnosticInfo",
    "default link @comment.todo    Todo",
    "@markup.strong        gui=bold          cterm=bold",
    "@markup.italic        gui=italic        cterm=italic",
    "@markup.strikethrough gui=strikethrough cterm=strikethrough",
    "@markup.underline     gui=underline     cterm=underline",
    "default link @markup         Special",
    "default link @markup.heading Title",
    "default link @markup.link    Underlined",
    "default link @diff.plus  Added",
    "default link @diff.minus Removed",
    "default link @diff.delta Changed",
    "default link @tag         Tag",
    "default link @tag.builtin Special",
    "default @markup.heading.1.delimiter.vimdoc guibg=bg guifg=bg guisp=fg gui=underdouble,nocombine ctermbg=NONE ctermfg=NONE cterm=underdouble,nocombine",
    "default @markup.heading.2.delimiter.vimdoc guibg=bg guifg=bg guisp=fg gui=underline,nocombine ctermbg=NONE ctermfg=NONE cterm=underline,nocombine",
    "default link @lsp.type.class         @type",
    "default link @lsp.type.comment       @comment",
    "default link @lsp.type.decorator     @attribute",
    "default link @lsp.type.enum          @type",
    "default link @lsp.type.enumMember    @constant",
    "default link @lsp.type.event         @type",
    "default link @lsp.type.function      @function",
    "default link @lsp.type.interface     @type",
    "default link @lsp.type.keyword       @keyword",
    "default link @lsp.type.macro         @constant.macro",
    "default link @lsp.type.method        @function.method",
    "default link @lsp.type.modifier      @type.qualifier",
    "default link @lsp.type.namespace     @module",
    "default link @lsp.type.number        @number",
    "default link @lsp.type.operator      @operator",
    "default link @lsp.type.parameter     @variable.parameter",
    "default link @lsp.type.property      @property",
    "default link @lsp.type.regexp        @string.regexp",
    "default link @lsp.type.string        @string",
    "default link @lsp.type.struct        @type",
    "default link @lsp.type.type          @type",
    "default link @lsp.type.typeParameter @type.definition",
    "default link @lsp.type.variable      @variable",
    "default link @lsp.mod.deprecated DiagnosticDeprecated",
];

/// Groups defined when `background` is light.
pub const HIGHLIGHT_INIT_LIGHT: &[&str] = &[
    "Normal guifg=NvimDarkGrey2 guibg=NvimLightGrey2 ctermfg=NONE ctermbg=NONE",
    "Added                guifg=NvimDarkGreen                                  ctermfg=2",
    "Changed              guifg=NvimDarkCyan                                   ctermfg=6",
    "ColorColumn                               guibg=NvimLightGrey4            cterm=reverse",
    "Conceal              guifg=NvimLightGrey4",
    "CurSearch            guifg=NvimLightGrey1 guibg=NvimDarkYellow            ctermfg=15 ctermbg=3",
    "CursorColumn                              guibg=NvimLightGrey3",
    "CursorLine                                guibg=NvimLightGrey3",
    "DiffAdd              guifg=NvimDarkGrey1  guibg=NvimLightGreen            ctermfg=15 ctermbg=2",
    "DiffChange           guifg=NvimDarkGrey1  guibg=NvimLightGrey4",
    "DiffDelete           guifg=NvimDarkRed                          gui=bold  ctermfg=1 cterm=bold",
    "DiffText             guifg=NvimDarkGrey1  guibg=NvimLightCyan             ctermfg=15 ctermbg=6",
    "Directory            guifg=NvimDarkCyan                                   ctermfg=6",
    "ErrorMsg             guifg=NvimDarkRed                                    ctermfg=1",
    "FloatShadow                               guibg=NvimLightGrey4            ctermbg=0 blend=80",
    "FloatShadowThrough                        guibg=NvimLightGrey4            ctermbg=0 blend=100",
    "Folded               guifg=NvimDarkGrey4  guibg=NvimLightGrey1",
    "LineNr               guifg=NvimLightGrey4",
    "MatchParen                                guibg=NvimLightGrey4  gui=bold  cterm=bold,underline",
    "ModeMsg              guifg=NvimDarkGreen                                  ctermfg=2",
    "MoreMsg              guifg=NvimDarkCyan                                   ctermfg=6",
    "NonText              guifg=NvimLightGrey4",
    "NormalFloat                               guibg=NvimLightGrey1",
    "OkMsg                guifg=NvimDarkGreen                                  ctermfg=2",
    "Pmenu                                     guibg=NvimLightGrey3            cterm=reverse",
    "PmenuThumb                                guibg=NvimLightGrey4",
    "Question             guifg=NvimDarkCyan                                   ctermfg=6",
    "QuickFixLine         guifg=NvimDarkCyan                                   ctermfg=6",
    "RedrawDebugClear                          guibg=NvimLightYellow           ctermfg=15 ctermbg=3",
    "RedrawDebugComposed                       guibg=NvimLightGreen            ctermfg=15 ctermbg=2",
    "RedrawDebugRecompose                      guibg=NvimLightRed              ctermfg=15 ctermbg=1",
    "Removed              guifg=NvimDarkRed                                    ctermfg=1",
    "Search               guifg=NvimDarkGrey1  guibg=NvimLightYellow           ctermfg=15 ctermbg=3",
    "SignColumn           guifg=NvimLightGrey4",
    "SpellBad             guisp=NvimDarkRed    gui=undercurl                   cterm=undercurl",
    "SpellCap             guisp=NvimDarkYellow gui=undercurl                   cterm=undercurl",
    "SpellLocal           guisp=NvimDarkGreen  gui=undercurl                   cterm=undercurl",
    "SpellRare            guisp=NvimDarkCyan   gui=undercurl                   cterm=undercurl",
    "StatusLine           guifg=NvimDarkGrey2  guibg=NvimLightGrey4            cterm=reverse",
    "StatusLineNC         guifg=NvimDarkGrey3  guibg=NvimLightGrey3            cterm=bold,underline",
    "Title                guifg=NvimDarkGrey2                        gui=bold  cterm=bold",
    "Visual                                    guibg=NvimLightGrey4            ctermfg=15 ctermbg=0",
    "WarningMsg           guifg=NvimDarkYellow                                 ctermfg=3",
    "WinBar               guifg=NvimDarkGrey4  guibg=NvimLightGrey1  gui=bold  cterm=bold",
    "WinBarNC             guifg=NvimDarkGrey4  guibg=NvimLightGrey1            cterm=bold",
    "Constant   guifg=NvimDarkGrey2",
    "Operator   guifg=NvimDarkGrey2",
    "PreProc    guifg=NvimDarkGrey2",
    "Type       guifg=NvimDarkGrey2",
    "Delimiter  guifg=NvimDarkGrey2",
    "Comment    guifg=NvimDarkGrey4",
    "String     guifg=NvimDarkGreen                    ctermfg=2",
    "Identifier guifg=NvimDarkBlue                     ctermfg=4",
    "Function   guifg=NvimDarkCyan                     ctermfg=6",
    "Statement  guifg=NvimDarkGrey2 gui=bold           cterm=bold",
    "Special    guifg=NvimDarkCyan                     ctermfg=6",
    "Error      guifg=NvimDarkGrey1 guibg=NvimLightRed ctermfg=15 ctermbg=1",
    "Todo       guifg=NvimDarkGrey2 gui=bold           cterm=bold",
    "DiagnosticError          guifg=NvimDarkRed                      ctermfg=1",
    "DiagnosticWarn           guifg=NvimDarkYellow                   ctermfg=3",
    "DiagnosticInfo           guifg=NvimDarkCyan                     ctermfg=6",
    "DiagnosticHint           guifg=NvimDarkBlue                     ctermfg=4",
    "DiagnosticOk             guifg=NvimDarkGreen                    ctermfg=2",
    "DiagnosticUnderlineError guisp=NvimDarkRed    gui=underline     cterm=underline",
    "DiagnosticUnderlineWarn  guisp=NvimDarkYellow gui=underline     cterm=underline",
    "DiagnosticUnderlineInfo  guisp=NvimDarkCyan   gui=underline     cterm=underline",
    "DiagnosticUnderlineHint  guisp=NvimDarkBlue   gui=underline     cterm=underline",
    "DiagnosticUnderlineOk    guisp=NvimDarkGreen  gui=underline     cterm=underline",
    "DiagnosticDeprecated     guisp=NvimDarkRed    gui=strikethrough cterm=strikethrough",
    "@variable guifg=NvimDarkGrey2",
];

/// Groups defined when `background` is dark.
pub const HIGHLIGHT_INIT_DARK: &[&str] = &[
    "Normal guifg=NvimLightGrey2 guibg=NvimDarkGrey2 ctermfg=NONE ctermbg=NONE",
    "Added                guifg=NvimLightGreen                                ctermfg=10",
    "Changed              guifg=NvimLightCyan                                 ctermfg=14",
    "ColorColumn                                guibg=NvimDarkGrey4           cterm=reverse",
    "Conceal              guifg=NvimDarkGrey4",
    "CurSearch            guifg=NvimDarkGrey1   guibg=NvimLightYellow         ctermfg=0 ctermbg=11",
    "CursorColumn                               guibg=NvimDarkGrey3",
    "CursorLine                                 guibg=NvimDarkGrey3",
    "DiffAdd              guifg=NvimLightGrey1  guibg=NvimDarkGreen           ctermfg=0 ctermbg=10",
    "DiffChange           guifg=NvimLightGrey1  guibg=NvimDarkGrey4",
    "DiffDelete           guifg=NvimLightRed                         gui=bold ctermfg=9 cterm=bold",
    "DiffText             guifg=NvimLightGrey1  guibg=NvimDarkCyan            ctermfg=0 ctermbg=14",
    "Directory            guifg=NvimLightCyan                                 ctermfg=14",
    "ErrorMsg             guifg=NvimLightRed                                  ctermfg=9",
    "FloatShadow                                guibg=NvimDarkGrey4           ctermbg=0 blend=80",
    "FloatShadowThrough                         guibg=NvimDarkGrey4           ctermbg=0 blend=100",
    "Folded               guifg=NvimLightGrey4  guibg=NvimDarkGrey1",
    "LineNr               guifg=NvimDarkGrey4",
    "MatchParen                                 guibg=NvimDarkGrey4  gui=bold cterm=bold,underline",
    "ModeMsg              guifg=NvimLightGreen                                ctermfg=10",
    "MoreMsg              guifg=NvimLightCyan                                 ctermfg=14",
    "NonText              guifg=NvimDarkGrey4",
    "NormalFloat                                guibg=NvimDarkGrey1",
    "OkMsg                guifg=NvimLightGreen                                ctermfg=10",
    "Pmenu                                      guibg=NvimDarkGrey3           cterm=reverse",
    "PmenuThumb                                 guibg=NvimDarkGrey4",
    "Question             guifg=NvimLightCyan                                 ctermfg=14",
    "QuickFixLine         guifg=NvimLightCyan                                 ctermfg=14",
    "RedrawDebugClear                           guibg=NvimDarkYellow          ctermfg=0 ctermbg=11",
    "RedrawDebugComposed                        guibg=NvimDarkGreen           ctermfg=0 ctermbg=10",
    "RedrawDebugRecompose                       guibg=NvimDarkRed             ctermfg=0 ctermbg=9",
    "Removed              guifg=NvimLightRed                                  ctermfg=9",
    "Search               guifg=NvimLightGrey1  guibg=NvimDarkYellow          ctermfg=0 ctermbg=11",
    "SignColumn           guifg=NvimDarkGrey4",
    "SpellBad             guisp=NvimLightRed    gui=undercurl                 cterm=undercurl",
    "SpellCap             guisp=NvimLightYellow gui=undercurl                 cterm=undercurl",
    "SpellLocal           guisp=NvimLightGreen  gui=undercurl                 cterm=undercurl",
    "SpellRare            guisp=NvimLightCyan   gui=undercurl                 cterm=undercurl",
    "StatusLine           guifg=NvimLightGrey2  guibg=NvimDarkGrey4           cterm=reverse",
    "StatusLineNC         guifg=NvimLightGrey3  guibg=NvimDarkGrey3           cterm=bold,underline",
    "Title                guifg=NvimLightGrey2                       gui=bold cterm=bold",
    "Visual                                     guibg=NvimDarkGrey4           ctermfg=0 ctermbg=15",
    "WarningMsg           guifg=NvimLightYellow                               ctermfg=11",
    "WinBar               guifg=NvimLightGrey4  guibg=NvimDarkGrey1  gui=bold cterm=bold",
    "WinBarNC             guifg=NvimLightGrey4  guibg=NvimDarkGrey1           cterm=bold",
    "Constant   guifg=NvimLightGrey2",
    "Operator   guifg=NvimLightGrey2",
    "PreProc    guifg=NvimLightGrey2",
    "Type       guifg=NvimLightGrey2",
    "Delimiter  guifg=NvimLightGrey2",
    "Comment    guifg=NvimLightGrey4",
    "String     guifg=NvimLightGreen                   ctermfg=10",
    "Identifier guifg=NvimLightBlue                    ctermfg=12",
    "Function   guifg=NvimLightCyan                    ctermfg=14",
    "Statement  guifg=NvimLightGrey2 gui=bold          cterm=bold",
    "Special    guifg=NvimLightCyan                    ctermfg=14",
    "Error      guifg=NvimLightGrey1 guibg=NvimDarkRed ctermfg=0 ctermbg=9",
    "Todo       guifg=NvimLightGrey2 gui=bold          cterm=bold",
    "DiagnosticError          guifg=NvimLightRed                      ctermfg=9",
    "DiagnosticWarn           guifg=NvimLightYellow                   ctermfg=11",
    "DiagnosticInfo           guifg=NvimLightCyan                     ctermfg=14",
    "DiagnosticHint           guifg=NvimLightBlue                     ctermfg=12",
    "DiagnosticOk             guifg=NvimLightGreen                    ctermfg=10",
    "DiagnosticUnderlineError guisp=NvimLightRed    gui=underline     cterm=underline",
    "DiagnosticUnderlineWarn  guisp=NvimLightYellow gui=underline     cterm=underline",
    "DiagnosticUnderlineInfo  guisp=NvimLightCyan   gui=underline     cterm=underline",
    "DiagnosticUnderlineHint  guisp=NvimLightBlue   gui=underline     cterm=underline",
    "DiagnosticUnderlineOk    guisp=NvimLightGreen  gui=underline     cterm=underline",
    "DiagnosticDeprecated     guisp=NvimLightRed    gui=strikethrough cterm=strikethrough",
    "@variable guifg=NvimLightGrey2",
];

/// Groups for command-line highlighting, verbatim from upstream
/// `highlight_init_cmdline` (highlight_group.c), applied by
/// `init_highlight` via `syn_init_cmdline_highlight`. Every name and link
/// target below was verified against the oracle's fresh-startup `:highlight`
/// listing; `NvimInternalError` keeps the upstream `Red` spelling and the
/// query renderer prints the terminal number.
pub const HIGHLIGHT_INIT_CMDLINE: &[&str] = &[
    "NvimInternalError ctermfg=Red ctermbg=Red guifg=Red guibg=Red",
    "default link NvimAssignment Operator",
    "default link NvimPlainAssignment NvimAssignment",
    "default link NvimAugmentedAssignment NvimAssignment",
    "default link NvimAssignmentWithAddition NvimAugmentedAssignment",
    "default link NvimAssignmentWithSubtraction NvimAugmentedAssignment",
    "default link NvimAssignmentWithConcatenation NvimAugmentedAssignment",
    "default link NvimOperator Operator",
    "default link NvimUnaryOperator NvimOperator",
    "default link NvimUnaryPlus NvimUnaryOperator",
    "default link NvimUnaryMinus NvimUnaryOperator",
    "default link NvimNot NvimUnaryOperator",
    "default link NvimBinaryOperator NvimOperator",
    "default link NvimComparison NvimBinaryOperator",
    "default link NvimComparisonModifier NvimComparison",
    "default link NvimBinaryPlus NvimBinaryOperator",
    "default link NvimBinaryMinus NvimBinaryOperator",
    "default link NvimConcat NvimBinaryOperator",
    "default link NvimConcatOrSubscript NvimConcat",
    "default link NvimOr NvimBinaryOperator",
    "default link NvimAnd NvimBinaryOperator",
    "default link NvimMultiplication NvimBinaryOperator",
    "default link NvimDivision NvimBinaryOperator",
    "default link NvimMod NvimBinaryOperator",
    "default link NvimTernary NvimOperator",
    "default link NvimTernaryColon NvimTernary",
    "default link NvimParenthesis Delimiter",
    "default link NvimLambda NvimParenthesis",
    "default link NvimNestingParenthesis NvimParenthesis",
    "default link NvimCallingParenthesis NvimParenthesis",
    "default link NvimSubscript NvimParenthesis",
    "default link NvimSubscriptBracket NvimSubscript",
    "default link NvimSubscriptColon NvimSubscript",
    "default link NvimCurly NvimSubscript",
    "default link NvimContainer NvimParenthesis",
    "default link NvimDict NvimContainer",
    "default link NvimList NvimContainer",
    "default link NvimIdentifier Identifier",
    "default link NvimIdentifierScope NvimIdentifier",
    "default link NvimIdentifierScopeDelimiter NvimIdentifier",
    "default link NvimIdentifierName NvimIdentifier",
    "default link NvimIdentifierKey NvimIdentifier",
    "default link NvimColon Delimiter",
    "default link NvimComma Delimiter",
    "default link NvimArrow Delimiter",
    "default link NvimRegister SpecialChar",
    "default link NvimNumber Number",
    "default link NvimFloat NvimNumber",
    "default link NvimNumberPrefix Type",
    "default link NvimOptionSigil Type",
    "default link NvimOptionName NvimIdentifier",
    "default link NvimOptionScope NvimIdentifierScope",
    "default link NvimOptionScopeDelimiter NvimIdentifierScopeDelimiter",
    "default link NvimEnvironmentSigil NvimOptionSigil",
    "default link NvimEnvironmentName NvimIdentifier",
    "default link NvimString String",
    "default link NvimStringBody NvimString",
    "default link NvimStringQuote NvimString",
    "default link NvimStringSpecial SpecialChar",
    "default link NvimSingleQuote NvimStringQuote",
    "default link NvimSingleQuotedBody NvimStringBody",
    "default link NvimSingleQuotedQuote NvimStringSpecial",
    "default link NvimDoubleQuote NvimStringQuote",
    "default link NvimDoubleQuotedBody NvimStringBody",
    "default link NvimDoubleQuotedEscape NvimStringSpecial",
    "default link NvimFigureBrace NvimInternalError",
    "default link NvimSingleQuotedUnknownEscape NvimInternalError",
    "default link NvimSpacing Normal",
    "default link NvimInvalidSingleQuotedUnknownEscape NvimInternalError",
    "default link NvimInvalid Error",
    "default link NvimInvalidAssignment NvimInvalid",
    "default link NvimInvalidPlainAssignment NvimInvalidAssignment",
    "default link NvimInvalidAugmentedAssignment NvimInvalidAssignment",
    "default link NvimInvalidAssignmentWithAddition NvimInvalidAugmentedAssignment",
    "default link NvimInvalidAssignmentWithSubtraction NvimInvalidAugmentedAssignment",
    "default link NvimInvalidAssignmentWithConcatenation NvimInvalidAugmentedAssignment",
    "default link NvimInvalidOperator NvimInvalid",
    "default link NvimInvalidUnaryOperator NvimInvalidOperator",
    "default link NvimInvalidUnaryPlus NvimInvalidUnaryOperator",
    "default link NvimInvalidUnaryMinus NvimInvalidUnaryOperator",
    "default link NvimInvalidNot NvimInvalidUnaryOperator",
    "default link NvimInvalidBinaryOperator NvimInvalidOperator",
    "default link NvimInvalidComparison NvimInvalidBinaryOperator",
    "default link NvimInvalidComparisonModifier NvimInvalidComparison",
    "default link NvimInvalidBinaryPlus NvimInvalidBinaryOperator",
    "default link NvimInvalidBinaryMinus NvimInvalidBinaryOperator",
    "default link NvimInvalidConcat NvimInvalidBinaryOperator",
    "default link NvimInvalidConcatOrSubscript NvimInvalidConcat",
    "default link NvimInvalidOr NvimInvalidBinaryOperator",
    "default link NvimInvalidAnd NvimInvalidBinaryOperator",
    "default link NvimInvalidMultiplication NvimInvalidBinaryOperator",
    "default link NvimInvalidDivision NvimInvalidBinaryOperator",
    "default link NvimInvalidMod NvimInvalidBinaryOperator",
    "default link NvimInvalidTernary NvimInvalidOperator",
    "default link NvimInvalidTernaryColon NvimInvalidTernary",
    "default link NvimInvalidDelimiter NvimInvalid",
    "default link NvimInvalidParenthesis NvimInvalidDelimiter",
    "default link NvimInvalidLambda NvimInvalidParenthesis",
    "default link NvimInvalidNestingParenthesis NvimInvalidParenthesis",
    "default link NvimInvalidCallingParenthesis NvimInvalidParenthesis",
    "default link NvimInvalidSubscript NvimInvalidParenthesis",
    "default link NvimInvalidSubscriptBracket NvimInvalidSubscript",
    "default link NvimInvalidSubscriptColon NvimInvalidSubscript",
    "default link NvimInvalidCurly NvimInvalidSubscript",
    "default link NvimInvalidContainer NvimInvalidParenthesis",
    "default link NvimInvalidDict NvimInvalidContainer",
    "default link NvimInvalidList NvimInvalidContainer",
    "default link NvimInvalidValue NvimInvalid",
    "default link NvimInvalidIdentifier NvimInvalidValue",
    "default link NvimInvalidIdentifierScope NvimInvalidIdentifier",
    "default link NvimInvalidIdentifierScopeDelimiter NvimInvalidIdentifier",
    "default link NvimInvalidIdentifierName NvimInvalidIdentifier",
    "default link NvimInvalidIdentifierKey NvimInvalidIdentifier",
    "default link NvimInvalidColon NvimInvalidDelimiter",
    "default link NvimInvalidComma NvimInvalidDelimiter",
    "default link NvimInvalidArrow NvimInvalidDelimiter",
    "default link NvimInvalidRegister NvimInvalidValue",
    "default link NvimInvalidNumber NvimInvalidValue",
    "default link NvimInvalidFloat NvimInvalidNumber",
    "default link NvimInvalidNumberPrefix NvimInvalidNumber",
    "default link NvimInvalidOptionSigil NvimInvalidIdentifier",
    "default link NvimInvalidOptionName NvimInvalidIdentifier",
    "default link NvimInvalidOptionScope NvimInvalidIdentifierScope",
    "default link NvimInvalidOptionScopeDelimiter NvimInvalidIdentifierScopeDelimiter",
    "default link NvimInvalidEnvironmentSigil NvimInvalidOptionSigil",
    "default link NvimInvalidEnvironmentName NvimInvalidIdentifier",
    "default link NvimInvalidString NvimInvalidValue",
    "default link NvimInvalidStringBody NvimStringBody",
    "default link NvimInvalidStringQuote NvimInvalidString",
    "default link NvimInvalidStringSpecial NvimStringSpecial",
    "default link NvimInvalidSingleQuote NvimInvalidStringQuote",
    "default link NvimInvalidSingleQuotedBody NvimInvalidStringBody",
    "default link NvimInvalidSingleQuotedQuote NvimInvalidStringSpecial",
    "default link NvimInvalidDoubleQuote NvimInvalidStringQuote",
    "default link NvimInvalidDoubleQuotedBody NvimInvalidStringBody",
    "default link NvimInvalidDoubleQuotedEscape NvimInvalidStringSpecial",
    "default link NvimInvalidDoubleQuotedUnknownEscape NvimInvalidValue",
    "default link NvimInvalidFigureBrace NvimInvalidDelimiter",
    "default link NvimInvalidSpacing ErrorMsg",
    "default link NvimDoubleQuotedUnknownEscape NvimInvalidValue",
];

/// The `hlf_names` table (highlight.h) interned at startup: every UI context
/// name exists even when no init list defines it. All but `VisualNC` are
/// covered by the lists above; it is pinned here so the table matches the
/// oracle's fresh-startup set exactly.
const HIGHLIGHT_INIT_INTERNED: &[&str] = &[
    "SpecialKey",
    "EndOfBuffer",
    "TermCursor",
    "NonText",
    "Directory",
    "OkMsg",
    "WarningMsg",
    "ErrorMsg",
    "StderrMsg",
    "StdoutMsg",
    "IncSearch",
    "Search",
    "CurSearch",
    "MoreMsg",
    "ModeMsg",
    "LineNr",
    "LineNrAbove",
    "LineNrBelow",
    "CursorLineNr",
    "CursorLineSign",
    "CursorLineFold",
    "Question",
    "StatusLine",
    "StatusLineNC",
    "WinSeparator",
    "Title",
    "Visual",
    "VisualNC",
    "VertSplit",
    "WildMenu",
    "Folded",
    "FoldColumn",
    "DiffAdd",
    "DiffChange",
    "DiffDelete",
    "DiffText",
    "DiffTextAdd",
    "SignColumn",
    "Conceal",
    "SpellBad",
    "SpellCap",
    "SpellRare",
    "SpellLocal",
    "Pmenu",
    "PmenuSel",
    "PmenuMatch",
    "PmenuMatchSel",
    "PmenuKind",
    "PmenuKindSel",
    "PmenuExtra",
    "PmenuExtraSel",
    "PmenuSbar",
    "PmenuThumb",
    "TabLine",
    "TabLineSel",
    "TabLineFill",
    "CursorColumn",
    "CursorLine",
    "ColorColumn",
    "QuickFixLine",
    "Whitespace",
    "NormalNC",
    "MsgSeparator",
    "NormalFloat",
    "MsgArea",
    "FloatBorder",
    "WinBar",
    "WinBarNC",
    "Cursor",
    "FloatTitle",
    "FloatFooter",
    "StatusLineTerm",
    "StatusLineTermNC",
    "PreInsert",
    "PmenuBorder",
];

use crate::Editor;
use crate::excmd_exec::apply_highlight_spec;

/// Seeds the editor's highlight table the way `init_highlight`
/// (highlight_group.c) seeds `hl_table` from `main()`: the `both` list plus
/// the list for the current `background`, applied through the same spec
/// parser the `:highlight` command uses, followed by the command-line list
/// (`syn_init_cmdline_highlight`).
///
/// Errors are impossible for the checked-in verbatim strings and are pinned
/// by `startup_highlight_specs_all_apply`; a failure at runtime would be a
/// build bug, so it is skipped loudly under debug builds.
pub fn init_highlight(editor: &mut Editor) {
    let light = matches!(
        editor.options().get_global("background"),
        Ok(crate::options::OptionValue::String(value)) if value == "light"
    );
    let lists: &[&[&str]] = if light {
        &[
            HIGHLIGHT_INIT_BOTH,
            HIGHLIGHT_INIT_LIGHT,
            HIGHLIGHT_INIT_CMDLINE,
        ]
    } else {
        &[
            HIGHLIGHT_INIT_BOTH,
            HIGHLIGHT_INIT_DARK,
            HIGHLIGHT_INIT_CMDLINE,
        ]
    };
    for list in lists {
        for spec in *list {
            if let Err((code, message)) = apply_highlight_spec(editor, spec) {
                debug_assert!(
                    false,
                    "startup highlight spec rejected: {spec} ({code}: {message})"
                );
            }
        }
    }
    ensure_startup_aliases(editor);
}

/// Completes the startup table the way upstream `syn_check_group` interning
/// does: every link target exists (a bare target lists as `cleared`), every
/// `@`-scoped parent exists (`syn_add_group` creates dot-parents for
/// `@`-names), and every `hlf_names` entry exists. Only missing entries are
/// added, always empty, so existing definitions are never disturbed.
fn ensure_startup_aliases(editor: &mut Editor) {
    let mut missing: Vec<String> = Vec::new();
    let known = |editor: &Editor, name: &str| {
        editor
            .highlights()
            .keys()
            .any(|candidate| candidate.eq_ignore_ascii_case(name))
    };
    for (name, attributes) in editor.highlights().iter() {
        if let Some(target) = attributes.get("link")
            && !target.eq_ignore_ascii_case("NONE")
            && !known(editor, target)
        {
            missing.push(target.clone());
        }
        if let Some(scoped) = name.strip_prefix('@') {
            let mut rest = scoped.to_owned();
            while let Some((parent, _)) = rest.rsplit_once('.') {
                let parent = format!("@{parent}");
                if !known(editor, &parent) {
                    missing.push(parent.clone());
                }
                rest = parent.strip_prefix('@').unwrap_or_default().to_owned();
            }
        }
    }
    for name in HIGHLIGHT_INIT_INTERNED {
        if !known(editor, name) {
            missing.push((*name).to_owned());
        }
    }
    for name in missing {
        if !known(editor, &name) {
            editor.highlights_mut().insert(name, Default::default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Editor;

    // The checked-in specs are verbatim upstream strings; any rejection is a
    // transcription bug, and init_highlight would silently skip the group.
    #[test]
    fn startup_highlight_specs_all_apply() {
        for list in [
            HIGHLIGHT_INIT_BOTH,
            HIGHLIGHT_INIT_LIGHT,
            HIGHLIGHT_INIT_DARK,
            HIGHLIGHT_INIT_CMDLINE,
        ] {
            let mut editor = Editor::new();
            for spec in list {
                apply_highlight_spec(&mut editor, spec)
                    .unwrap_or_else(|(code, message)| panic!("{spec}: {code} {message}"));
            }
        }
    }

    // hlID()/hlexists() see the startup table (syn_name2id over hl_table).
    #[test]
    fn startup_groups_present_on_fresh_editor() {
        let editor = Editor::new();
        for group in [
            "NonText",
            "Normal",
            "StatusLine",
            "PmenuKind",
            "VisualNC",
            "NvimAssignment",
            "NvimInvalidSpacing",
        ] {
            assert!(
                editor.highlights().contains_key(group),
                "startup group {group} missing"
            );
        }
    }

    // The command-line list, `@`-parents, and HLF-interned names complete
    // the fresh table to the oracle's startup set (388 listed rows plus the
    // 11 listed-but-skipped `@` groups).
    #[test]
    fn startup_table_matches_oracle_fresh_set() {
        let editor = Editor::new();
        assert_eq!(editor.highlights().len(), 399);
        // `@`-parents exist even though the bare listing skips them.
        for group in [
            "@diff",
            "@lsp",
            "@lsp.type",
            "@function.method",
            "@type.definition",
            "@markup.heading.1.delimiter",
        ] {
            assert!(
                editor.highlights().contains_key(group),
                "startup alias {group} missing"
            );
            assert!(
                editor.highlights()[group].is_empty(),
                "startup alias {group} should be empty (cleared)"
            );
        }
        // Link targets of the cmdline list exist with their oracle targets.
        assert_eq!(
            editor
                .highlights()
                .get("NvimSpacing")
                .and_then(|attrs| attrs.get("link"))
                .map(String::as_str),
            Some("Normal")
        );
        assert_eq!(
            editor
                .highlights()
                .get("NvimInvalidSpacing")
                .and_then(|attrs| attrs.get("link"))
                .map(String::as_str),
            Some("ErrorMsg")
        );
        assert_eq!(
            editor
                .highlights()
                .get("VisualNC")
                .map(|attrs| attrs.is_empty()),
            Some(true)
        );
    }
}
