#!/usr/bin/env python3
"""会话身份轮换的源码静态检查；不编译、不启动应用、不连接上游。"""

import argparse
import importlib.metadata
import json
from pathlib import Path
import sys
import zipfile


FILES = [
    "src/types.ts",
    "src/components/providers/forms/ProviderForm.tsx",
    "src/components/providers/forms/CodexFormFields.tsx",
    "tests/components/ProviderForm.codexSessionIdentity.test.tsx",
    "src-tauri/src/provider.rs",
    "src-tauri/src/database/dao/providers_tests.rs",
    "src-tauri/src/proxy/mod.rs",
    "src-tauri/src/proxy/codex_history_archive.rs",
    "src-tauri/src/proxy/codex_session_identity.rs",
    "src-tauri/src/proxy/codex_session_identity/tests.rs",
    "src-tauri/src/proxy/forwarder.rs",
    "src-tauri/src/proxy/forwarder/tests.rs",
    "src-tauri/src/proxy/forwarder/session_identity.rs",
    "src-tauri/src/proxy/forwarder/session_identity_notice.rs",
    "src-tauri/src/proxy/forwarder/session_identity_notice_response.rs",
    "src-tauri/src/proxy/forwarder/session_identity_tests.rs",
    "src-tauri/examples/session-continuation-probe.rs",
]


def syntax_errors(parser, data):
    tree = parser.parse(data)
    stack = [tree.root_node]
    errors = []
    while stack:
        node = stack.pop()
        if node.type == "ERROR" or node.is_missing:
            errors.append({"kind": node.type, "line": node.start_point.row + 1,
                           "column": node.start_point.column + 1})
        stack.extend(reversed(node.children))
    return errors


def main():
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
    args_parser = argparse.ArgumentParser(description=__doc__)
    args_parser.add_argument("--parser-path", type=Path, help="独立安装的静态解析器目录，可选")
    args_parser.add_argument("--baseline-zip", type=Path, help="原上传包；用于确认文件没有遗漏或删除")
    args_parser.add_argument("--output", type=Path, help="保存 JSON 检查结果")
    args = args_parser.parse_args()
    if args.parser_path:
        sys.path.insert(0, str(args.parser_path.resolve()))
    try:
        import tree_sitter
        import tree_sitter_rust
        import tree_sitter_typescript
    except ImportError:
        args_parser.error("请先安装 tree-sitter、tree-sitter-rust、tree-sitter-typescript 预编译包，详见中文说明")
    root = Path(__file__).resolve().parent.parent
    languages = {
        ".rs": tree_sitter.Language(tree_sitter_rust.language()),
        ".ts": tree_sitter.Language(tree_sitter_typescript.language_typescript()),
        ".tsx": tree_sitter.Language(tree_sitter_typescript.language_tsx()),
    }
    results = []
    for name in FILES:
        path = root / name
        if not path.is_file():
            results.append({"file": name, "missing": True})
            continue
        data = path.read_bytes()
        # UTF-8 解码单独检查，避免交付乱码被语法解析掩盖。
        data.decode("utf-8-sig")
        errors = syntax_errors(tree_sitter.Parser(languages[path.suffix]), data)
        results.append({"file": name, "syntax_errors": errors})
    result = {
        "说明": "仅语法树及源码完整性检查；不代表类型检查、测试执行、编译或真实上游验证",
        "parser_versions": {p: importlib.metadata.version(p) for p in
                            ["tree-sitter", "tree-sitter-rust", "tree-sitter-typescript"]},
        "files": results,
        "编译或运行测试": False,
        "真实上游请求": False,
    }
    failed = any(r.get("missing") or r.get("syntax_errors") for r in results)
    # 新增结构字段后，静态检查所有显式构造器是否通过 ..Default 等更新语法继承缺省。
    constructor_counts = {"ProviderMeta": 0, "RequestForwarder": 0}
    incomplete = []
    rust_parser = tree_sitter.Parser(languages[".rs"])
    for path in (root / "src-tauri").rglob("*.rs"):
        data = path.read_bytes()
        stack = [rust_parser.parse(data).root_node]
        while stack:
            node = stack.pop()
            if node.type == "struct_expression":
                name_node = node.child_by_field_name("name")
                name = data[name_node.start_byte:name_node.end_byte].decode().split("::")[-1] if name_node else ""
                if name == "Self":
                    parent = node.parent
                    while parent and parent.type != "impl_item":
                        parent = parent.parent
                    impl_type = parent.child_by_field_name("type") if parent else None
                    if impl_type:
                        name = data[impl_type.start_byte:impl_type.end_byte].decode().split("::")[-1]
                if name in constructor_counts:
                    constructor_counts[name] += 1
                    source = data[node.start_byte:node.end_byte]
                    required = b"codex_session_identity_rotation" if name == "ProviderMeta" else b"session_identity_retried"
                    if b".." not in source and required not in source:
                        incomplete.append({"file": path.relative_to(root).as_posix(), "line": node.start_point.row + 1})
            stack.extend(node.children)
    result["constructor_audit"] = {"counts": constructor_counts, "potentially_incomplete": incomplete}
    failed |= bool(incomplete)
    if args.baseline_zip:
        with zipfile.ZipFile(args.baseline_zip) as archive:
            baseline_files = [i for i in archive.infolist() if not i.is_dir()]
            prefix = baseline_files[0].filename.split("/", 1)[0] + "/"
            missing = [i.filename[len(prefix):] for i in baseline_files
                       if not (root / i.filename[len(prefix):]).is_file()]
            result["baseline_file_count"] = len(baseline_files)
            result["missing_baseline_files"] = missing
            failed |= bool(missing)
    result["结果"] = "失败" if failed else "通过"
    output = json.dumps(result, ensure_ascii=False, indent=2) + "\n"
    print(output)
    if args.output:
        with args.output.open("w", encoding="utf-8", newline="\n") as handle:
            handle.write(output)
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
