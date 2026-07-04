from enum import Enum


class Type(Enum):
    Number = "Number"
    String = "String"
    Array = "Array"
    Dict = "Dict"
    Object = "Object"
    Stream = "Stream"
    VecNum = "SmallVec<[Number; OPERANDS_THRESHOLD]>"
    Name = "Name"


ops = {
    "Compatibility operators": [
        ("BX", "BeginCompatibility", []),
        ("EX", "EndCompatibility", []),
    ],
    "Graphic state operators": [
        ("q", "SaveState", []),
        ("Q", "RestoreState", []),
        ("cm", "Transform", [Type.Number] * 6),
        ("w", "LineWidth", [Type.Number]),
        ("J", "LineCap", [Type.Number]),
        ("j", "LineJoin", [Type.Number]),
        ("M", "MiterLimit", [Type.Number]),
        ("d", "DashPattern", [Type.Array, Type.Number]),
        ("ri", "RenderingIntent", [Type.Name]),
        ("i", "FlatnessTolerance", [Type.Number]),
        ("gs", "SetGraphicsState", [Type.Name]),
    ],
    "Path-construction operators": [
        ("m", "MoveTo", [Type.Number, Type.Number]),
        ("l", "LineTo", [Type.Number, Type.Number]),
        ("c", "CubicTo", [Type.Number] * 6),
        ("v", "CubicStartTo", [Type.Number] * 4),
        ("y", "CubicEndTo", [Type.Number] * 4),
        ("h", "ClosePath", []),
        ("re", "RectPath", [Type.Number] * 4),
    ],
    "Path-painting operators": [
        ("S", "StrokePath", []),
        ("s", "CloseAndStrokePath", []),
        ("f", "FillPathNonZero", []),
        ("F", "FillPathNonZeroCompatibility", []),
        ("f*", "FillPathEvenOdd", []),
        ("B", "FillAndStrokeNonZero", []),
        ("B*", "FillAndStrokeEvenOdd", []),
        ("b", "CloseFillAndStrokeNonZero", []),
        ("b*", "CloseFillAndStrokeEvenOdd", []),
        ("n", "EndPath", []),
    ],
    "Clipping path operators": [
        ("W", "ClipNonZero", []),
        ("W*", "ClipEvenOdd", []),
    ],
    "Colour operators": [
        ("CS", "ColorSpaceStroke", [Type.Name]),
        ("cs", "ColorSpaceNonStroke", [Type.Name]),
        ("SC", "StrokeColor", [Type.VecNum]),
        ("SCN", "StrokeColorNamed", True),
        ("sc", "NonStrokeColor", [Type.VecNum]),
        ("scn", "NonStrokeColorNamed", True),
        ("G", "StrokeColorDeviceGray", [Type.Number]),
        ("g", "NonStrokeColorDeviceGray", [Type.Number]),
        ("RG", "StrokeColorDeviceRgb", [Type.Number] * 3),
        ("rg", "NonStrokeColorDeviceRgb", [Type.Number] * 3),
        ("K", "StrokeColorCmyk", [Type.Number] * 4),
        ("k", "NonStrokeColorCmyk", [Type.Number] * 4),
    ],
    "Shading operator": [("sh", "Shading", [Type.Name])],
    "XObject operator": [("Do", "XObject", [Type.Name])],
    "Inline-image operators": [
        (
            "BI",
            "InlineImage",
            [Type.Stream],
            (
                "Operator `BI` — begin inline image.\n"
                "\n"
                "The wrapped `&'b Stream<'a>` carries the image as a "
                "self-contained stream: use [`Stream::dict`] for the "
                "inline dictionary (width, height, colour space, "
                "bits-per-component, filter chain) and [`Stream::raw_data`] "
                "for the raw body bytes between `ID` and `EI`. The optional "
                "white-space delimiting the data from `EI` (ISO 32000-2 "
                "§8.9.7 NOTE 2) is not trimmed, so the bytes may carry one "
                "trailing delimiter byte beyond the image payload; filters "
                "and image decoders consume only what they need. "
                "[`Stream::decoded`] runs the declared filter "
                "chain.\n"
                "\n"
                "Inline image dictionaries use abbreviated keys per "
                "ISO 32000-1 §8.9.7.1 — e.g. `/W`, `/H`, `/CS`, `/BPC`, "
                "`/F`, `/DP`, `/D`, `/IM`, `/I`. Callers should not "
                "expand the abbreviations before reading entries.\n"
                "\n"
                "[`Stream::dict`]: crate::object::Stream::dict\n"
                "[`Stream::raw_data`]: crate::object::Stream::raw_data\n"
                "[`Stream::decoded`]: crate::object::Stream::decoded"
            ),
        ),
        # We do not emit ID and EI in the parser.
        # ("ID", "BeginInlineImageData", []),
        # ("EI", "EndInlineImage", []),
    ],
    "Text-state operators": [
        ("Tc", "CharacterSpacing", [Type.Number]),
        ("Tw", "WordSpacing", [Type.Number]),
        ("Tz", "HorizontalScaling", [Type.Number]),
        ("TL", "TextLeading", [Type.Number]),
        ("Tf", "TextFont", [Type.Name, Type.Number]),
        ("Tr", "TextRenderingMode", [Type.Number]),
        ("Ts", "TextRise", [Type.Number]),
    ],
    "Text-object operators": [
        ("BT", "BeginText", []),
        ("ET", "EndText", []),
    ],
    "Text-positioning operators": [
        ("Td", "NextLine", [Type.Number] * 2),
        ("TD", "NextLineAndSetLeading", [Type.Number] * 2),
        ("Tm", "SetTextMatrix", [Type.Number] * 6),
        ("T*", "NextLineUsingLeading", []),
    ],
    "Text-showing operators": [
        ("Tj", "ShowText", [Type.String]),
        ("'", "NextLineAndShowText", [Type.String]),
        ('"', "ShowTextWithParameters", [Type.Number, Type.Number, Type.String]),
        ("TJ", "ShowTexts", [Type.Array]),
    ],
    "Type 3 font operators": [
        ("d0", "ColorGlyph", [Type.Number] * 2),
        ("d1", "ShapeGlyph", [Type.Number] * 6),
    ],
    "Marked content operators": [
        ("MP", "MarkedContentPoint", [Type.Name]),
        # Second argument can be name or dict
        ("DP", "MarkedContentPointWithProperties", [Type.Name, Type.Object]),
        ("BMC", "BeginMarkedContent", [Type.Name]),
        # Second argument can be name or dict
        ("BDC", "BeginMarkedContentWithProperties", [Type.Name, Type.Object]),
        ("EMC", "EndMarkedContent", []),
    ],
}


def rust_type(t: Type) -> str:
    return {
        Type.Number: "Number",
        Type.String: "&'b object::String<'a>",
        Type.Array: "&'b Array<'a>",
        Type.Object: "&'b Object<'a>",
        Type.Stream: "&'b Stream<'a>",
        Type.Name: "&'b Name<'a>",
        Type.Dict: "&'b Dict<'a>",
        Type.VecNum: "SmallVec<[Number; OPERANDS_THRESHOLD]>",
    }[t]


def lifetime_if_needed(types):
    return (
        "<'b, 'a>"
        if any(
            t in [Type.Array, Type.Object, Type.Stream, Type.Dict, Type.Name, Type.String]
            for t in types
        )
        else ""
    )


def gen_struct(name, code, types, docs=None):
    lifetime = lifetime_if_needed(types)
    count = len(types)
    macro_suffix = count
    struct = []
    if docs:
        struct += [f"/// {line}" if line else "///" for line in docs.splitlines()]
    struct.append("#[derive(Debug, PartialEq, Clone)]")
    if count == 0:
        struct.append(f"pub struct {name};")
    elif count == 1:
        struct.append(f"pub struct {name}{lifetime}(pub {rust_type(types[0])});")
        if types[0] == Type.VecNum:
            macro_suffix = "_all"
    else:
        struct.append(f"pub struct {name}{lifetime}(")
        struct += [f"    pub {rust_type(t)}," for t in types]
        struct.append(");")
    # Escape the Rust string literal properly
    escaped_code = code.replace('"', '\\"')
    struct.append(f'op{macro_suffix}!({name}{lifetime}, "{escaped_code}");')
    return "\n".join(struct)


def gen_enum_variant(name, types, docs=None):
    has_lifetime = name in ["StrokeColorNamed", "NonStrokeColorNamed"] or (
        (type(types) is list)
        and any(t in [Type.Array, Type.Object, Type.Stream, Type.Name, Type.String] for t in types)
    )
    inner_type = f"{name}<'b, 'a>" if has_lifetime else name
    base = f"{name}({inner_type})"
    if not docs:
        return base
    # Leading `    ` indent on the first variant comes from the enum-block
    # template; subsequent lines (further doc lines + the variant itself)
    # must carry their own `    ` indent.
    lines = docs.splitlines()
    first = f"/// {lines[0]}" if lines[0] else "///"
    rest = [f"    /// {line}" if line else "    ///" for line in lines[1:]]
    rest.append(f"    {base}")
    return first + "\n" + "\n".join(rest)


def gen_dispatch_match(code, name, types):
    escaped_code = code.replace('"', '\\"')
    return f'b"{escaped_code}" => {name}::from_stack(instruction.operands)?.into(),'


# Generate all code pieces
structs = []
enum_variants = []
dispatch_arms = []

for category in ops.values():
    for entry in category:
        if len(entry) == 4:
            code, name, types, docs = entry
        else:
            code, name, types = entry
            docs = None
        if type(types) is list:
            structs.append(gen_struct(name, code, types, docs))
        enum_variants.append(gen_enum_variant(name, types, docs))
        dispatch_arms.append(gen_dispatch_match(code, name, types))

# Build the final Rust code blocks
struct_block = "\n\n".join(structs)

enum_block = (
    "#[derive(Debug, PartialEq, Clone)]\n"
    "pub enum TypedInstruction<'b, 'a> {\n" + "    " + ",\n    ".join(enum_variants) + ",\n"
    "    Fallback(&'b Operator<'a>),\n}"
)

dispatch_block = (
    "impl<'b, 'a> TypedInstruction<'b, 'a> {\n"
    "    #[inline(always)]\n"
    "    pub(crate) fn dispatch(instruction: &Instruction<'b, 'a>) -> Option<Self> {\n"
    "        let op_name = instruction.operator.as_ref();\n"
    "        Some(match op_name {\n"
    + "            "
    + "\n            ".join(dispatch_arms)
    + "\n"
    "            _ => return Some(Self::Fallback(instruction.operator)),\n"
    "        })\n"
    "    }\n"
    "}"
)

gen_notice = "// THIS FILE IS AUTO-GENERATED, DO NOT EDIT MANUALLY"
imports = "use crate::content::Operator;"

joined = "\n\n".join([gen_notice, imports, struct_block, enum_block, dispatch_block])

with open("ops_generated.rs", "w") as f:
    f.write(joined)
