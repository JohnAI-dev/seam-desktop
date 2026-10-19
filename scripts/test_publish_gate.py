#!/usr/bin/env python3
"""Decision tests for the release publish gate."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import publish_gate as gate

ROOT = Path(__file__).resolve().parents[1]

PROD = '''
fn shipped() -> i32 { 1 }

#[cfg(test)]
mod tests {
    fn scratch() { let _ = "helper-body"; }

    #[test]
    fn cfg_test_fn() { let _ = "seam-file-srv"; }
}
'''

PROD_WITH_INNER = '''
fn shipped() -> &'static str {
    let brace = '{';
    let end = "}";
    let _ = brace;
    end
}

#[tokio::test]
async fn only_a_test() { let _ = "seam-file-srv"; }
'''


def _manifest(deps: str, dev: str = "", build: str = "", target_dev: str = "") -> str:
    return f'''
[package]
name = "app"
version = "0.1.0"

[dependencies]
{deps}

[build-dependencies]
{build}

[dev-dependencies]
{dev}

[target.'cfg(windows)'.dev-dependencies]
{target_dev}
'''


def _lock(foo: str = "1.0.0", foo_sum: str = "bbb", bar: str | None = "1.0.0", bar_sum: str = "aaa") -> str:
    packages = f'''
[[package]]
name = "app"
version = "0.1.0"
dependencies = ["foo"{', "bar"' if bar else ""}]

[[package]]
name = "foo"
version = "{foo}"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "{foo_sum}"
'''
    if bar:
        packages += f'''
[[package]]
name = "bar"
version = "{bar}"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "{bar_sum}"
'''
    return "version = 3\n" + packages


class PublishGateTests(unittest.TestCase):
    def test_cfg_test_module_edit_does_not_publish(self) -> None:
        before = {"src/lib.rs": PROD}
        after = {"src/lib.rs": PROD.replace("seam-file-srv", "seam-file-srv-edited")}
        self.assertFalse(gate.should_publish(before, after))

    def test_tokio_test_fn_edit_does_not_publish(self) -> None:
        before = {"src/lib.rs": PROD_WITH_INNER}
        after = {"src/lib.rs": PROD_WITH_INNER.replace("seam-file-srv", "changed")}
        self.assertFalse(gate.should_publish(before, after))

    def test_cfg_all_test_is_stripped(self) -> None:
        src = '''
fn shipped() {}
#[cfg(all(test, feature = "extra"))]
fn hidden() { let _ = "only-test"; }
'''
        edited = src.replace("only-test", "changed")
        self.assertFalse(gate.should_publish({"a.rs": src}, {"a.rs": edited}))

    def test_cfg_any_test_is_kept(self) -> None:
        src = '''
#[cfg(any(test, feature = "extra"))]
fn maybe_shipped() { let _ = "maybe"; }
'''
        edited = src.replace("maybe", "changed")
        self.assertTrue(gate.should_publish({"a.rs": src}, {"a.rs": edited}))

    def test_cfg_attr_test_is_kept(self) -> None:
        src = '''
#[cfg_attr(test, allow(dead_code))]
fn shipped() { let _ = "keep"; }
'''
        self.assertTrue(
            gate.should_publish({"a.rs": src}, {"a.rs": src.replace("keep", "changed")})
        )

    def test_removing_cfg_test_publishes(self) -> None:
        after = PROD.replace("#[cfg(test)]\n", "", 1)
        # helper-body was test-only via the cfg(test) module. Dropping the
        # attribute ships it. The #[test] function stays stripped.
        self.assertIn("helper-body", gate.shippable_rust(after))
        self.assertNotIn("helper-body", gate.shippable_rust(PROD))
        self.assertNotIn("seam-file-srv", gate.shippable_rust(after))
        self.assertTrue(gate.should_publish({"src/lib.rs": PROD}, {"src/lib.rs": after}))

    def test_real_function_change_publishes(self) -> None:
        after = PROD.replace("fn shipped() -> i32 { 1 }", "fn shipped() -> i32 { 2 }")
        self.assertTrue(gate.should_publish({"src/lib.rs": PROD}, {"src/lib.rs": after}))

    def test_docs_readme_workflow_fixtures_do_not_publish(self) -> None:
        before = {
            "README.md": "old",
            "docs/guide.md": "old",
            "README": "old",
            ".github/workflows/release.yml": "old",
            "crates/app/src/fixtures/sample.json": "old",
            "crates/app/tests/link.rs": "fn changed_test() {}",
        }
        after = {path: "new" for path in before}
        self.assertFalse(gate.should_publish(before, after))

    def test_workflow_only_does_not_publish(self) -> None:
        self.assertFalse(
            gate.should_publish(
                {".github/workflows/ci.yml": "name: CI\n"},
                {".github/workflows/ci.yml": "name: CI changed\n"},
            )
        )

    def test_mixed_docs_and_code_publishes(self) -> None:
        before = {"README.md": "docs", "src/lib.rs": "fn shipped() {}\n"}
        after = {"README.md": "edited", "src/lib.rs": "fn shipped() { let _ = 1; }\n"}
        self.assertTrue(gate.should_publish(before, after))

    def test_gate_tooling_does_not_publish_updater_script_does(self) -> None:
        self.assertFalse(
            gate.should_publish(
                {"scripts/test.sh": "old", "scripts/publish_gate.py": "old"},
                {"scripts/test.sh": "new", "scripts/publish_gate.py": "new"},
            )
        )
        self.assertTrue(
            gate.should_publish(
                {"scripts/make_latest_json.py": "old"},
                {"scripts/make_latest_json.py": "new"},
            )
        )

    def test_unparseable_rust_publishes(self) -> None:
        self.assertTrue(
            gate.should_publish(
                {"src/lib.rs": "fn shipped() {}\n"},
                {"src/lib.rs": "this is not rust {{{"},
            )
        )

    def test_dependency_version_publishes(self) -> None:
        before = {
            "Cargo.toml": _manifest('foo = "1"\n', 'bar = "1"\n'),
            "Cargo.lock": _lock(),
        }
        after = {
            "Cargo.toml": _manifest('foo = "2"\n', 'bar = "1"\n'),
            "Cargo.lock": _lock(foo="2.0.0", foo_sum="ccc"),
        }
        self.assertTrue(gate.should_publish(before, after))

    def test_dev_dependency_only_does_not_publish(self) -> None:
        before = {
            "Cargo.toml": _manifest('foo = "1"\n', 'bar = "1"\n'),
            "Cargo.lock": _lock(),
        }
        after = {
            "Cargo.toml": _manifest('foo = "1"\n', 'bar = "2"\n'),
            "Cargo.lock": _lock(bar="2.0.0", bar_sum="ddd"),
        }
        self.assertFalse(gate.should_publish(before, after))

    def test_target_dev_dependency_does_not_publish(self) -> None:
        before = {
            "Cargo.toml": _manifest('foo = "1"\n', target_dev='baz = "1"\n'),
            "Cargo.lock": _lock(bar=None),
        }
        after = {
            "Cargo.toml": _manifest('foo = "1"\n', target_dev='baz = "2"\n'),
            "Cargo.lock": _lock(bar=None),
        }
        self.assertFalse(gate.should_publish(before, after))

    def test_build_dependency_publishes(self) -> None:
        before = {
            "Cargo.toml": _manifest('foo = "1"\n', build='builder = "1"\n'),
            "Cargo.lock": _lock(bar=None) + '''
[[package]]
name = "builder"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "eee"
''',
        }
        after = {
            "Cargo.toml": _manifest('foo = "1"\n', build='builder = "1"\n'),
            "Cargo.lock": _lock(bar=None) + '''
[[package]]
name = "builder"
version = "1.0.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "fff"
''',
        }
        # The manifest requirement is unchanged; the lock checksum must publish
        # because builder is a build-dependency, and app's lock edges only list
        # direct deps. Declare the edge explicitly.
        before["Cargo.lock"] = before["Cargo.lock"].replace(
            'dependencies = ["foo"]', 'dependencies = ["builder", "foo"]', 1
        )
        after["Cargo.lock"] = after["Cargo.lock"].replace(
            'dependencies = ["foo"]', 'dependencies = ["builder", "foo"]', 1
        )
        self.assertTrue(gate.should_publish(before, after))

    def test_lock_runtime_checksum_publishes(self) -> None:
        common = {"Cargo.toml": _manifest('foo = "1"\n')}
        before = {**common, "Cargo.lock": _lock(bar=None)}
        after = {**common, "Cargo.lock": _lock(bar=None, foo_sum="zzz")}
        self.assertTrue(gate.should_publish(before, after))

    def test_lock_dev_only_checksum_does_not_publish(self) -> None:
        common = {"Cargo.toml": _manifest('foo = "1"\n', 'bar = "1"\n')}
        before = {**common, "Cargo.lock": _lock()}
        after = {**common, "Cargo.lock": _lock(bar_sum="zzz")}
        self.assertFalse(gate.should_publish(before, after))

    def test_version_requirements(self) -> None:
        self.assertTrue(gate.version_matches("1.2.3", "1"))
        self.assertTrue(gate.version_matches("0.14.10", "0.14"))
        self.assertFalse(gate.version_matches("0.15.0", "0.14"))
        self.assertTrue(gate.version_matches("4.18.2", "=4.18.2"))
        self.assertFalse(gate.version_matches("4.18.3", "=4.18.2"))
        self.assertFalse(gate.version_matches("1.0.0-alpha", "1"))
        self.assertTrue(gate.version_matches("1.2.3", "~1.2"))
        self.assertFalse(gate.version_matches("1.3.0", "~1.2.3"))

    def test_repo_sources_strip_and_keep_production_strings(self) -> None:
        server = (ROOT / "crates/seam-core/src/link/server.rs").read_text(encoding="utf-8")
        stripped = gate.shippable_rust(server)
        self.assertIn("this phone is not connected", stripped)
        self.assertNotIn("seam-file-srv", stripped)
        self.assertNotIn("fn scratch", stripped)
        failures = []
        for path in ROOT.rglob("*.rs"):
            if "target" in path.parts:
                continue
            try:
                gate.shippable_rust(path.read_text(encoding="utf-8"))
            except gate.Uncertain as exc:
                failures.append(f"{path.relative_to(ROOT)}: {exc}")
        self.assertEqual(failures, [])

    def test_real_server_test_edit_does_not_publish_but_production_does(self) -> None:
        path = "crates/seam-core/src/link/server.rs"
        original = (ROOT / path).read_text(encoding="utf-8")
        test_edit = original.replace("seam-file-srv-{label}", "seam-file-srv-edited-{label}", 1)
        self.assertNotEqual(original, test_edit)
        self.assertFalse(gate.should_publish({path: original}, {path: test_edit}))
        prod_edit = original.replace(
            'Err("this phone is not connected".into())',
            'Err("this phone is offline".into())',
            1,
        )
        self.assertTrue(gate.should_publish({path: original}, {path: prod_edit}))

    def test_real_workspace_closure_includes_runtime_crates(self) -> None:
        files: dict[str, str | None] = {}
        for path in ROOT.rglob("Cargo.toml"):
            if "target" in path.parts:
                continue
            files[str(path.relative_to(ROOT))] = path.read_text(encoding="utf-8")
        files["Cargo.lock"] = (ROOT / "Cargo.lock").read_text(encoding="utf-8")
        rows = gate.lock_runtime_fingerprint(files)
        names = {(name, version) for name, version, _source, _sum in rows}
        self.assertIn(("hmac", "0.13.0"), names)
        self.assertIn(("sha2", "0.11.1"), names)
        self.assertIn(("rcgen", "0.14.10"), names)
        self.assertIn(("rand", "0.9.5"), names)
        self.assertGreater(len(rows), 400)

    def test_unreadable_path_publishes_even_when_both_sides_fail(self) -> None:
        missing = gate.TreeEntry(gate.Unreadable("cat-file failed"), "100644")
        self.assertTrue(
            gate.should_publish(
                {"src/lib.rs": missing},
                {"src/lib.rs": "fn shipped() {}\n"},
            )
        )
        self.assertTrue(gate.should_publish({"docs/a.md": missing}, {"docs/a.md": missing}))

    def test_mode_change_publishes_source_not_readme(self) -> None:
        src = "fn shipped() {}\n"
        self.assertTrue(
            gate.should_publish(
                {"src/lib.rs": gate.TreeEntry(src, "100644")},
                {"src/lib.rs": gate.TreeEntry(src, "100755")},
            )
        )
        self.assertFalse(
            gate.should_publish(
                {"README.md": gate.TreeEntry("docs\n", "100644")},
                {"README.md": gate.TreeEntry("docs\n", "100755")},
            )
        )

    def test_skip_is_limited_to_known_places(self) -> None:
        self.assertFalse(
            gate.should_publish(
                {"crates/app/tests/link.rs": "fn a() {}\n"},
                {"crates/app/tests/link.rs": "fn b() {}\n"},
            )
        )
        self.assertFalse(
            gate.should_publish(
                {"crates/app/src/fixtures/sample.json": "old"},
                {"crates/app/src/fixtures/sample.json": "new"},
            )
        )
        self.assertTrue(
            gate.should_publish(
                {"src/tests/foo.rs": "fn a() {}\n"},
                {"src/tests/foo.rs": "fn b() {}\n"},
            )
        )
        self.assertTrue(
            gate.should_publish(
                {"vendor/fixtures/sample.json": "old"},
                {"vendor/fixtures/sample.json": "new"},
            )
        )
        self.assertTrue(
            gate.should_publish(
                {"crates/app/README.md": "old"},
                {"crates/app/README.md": "new"},
            )
        )
        self.assertFalse(
            gate.should_publish({"README.md": "old"}, {"README.md": "new"})
        )

    def test_prod_include_is_not_skipped(self) -> None:
        src = 'fn shipped() { let _ = include_str!("../../../docs/guide.md"); }\n'
        before = {"crates/app/src/lib.rs": src, "docs/guide.md": "old"}
        after = {"crates/app/src/lib.rs": src, "docs/guide.md": "new"}
        self.assertTrue(gate.should_publish(before, after))
        raw = 'const ICON: &[u8] = include_bytes!(r"../icons/128x128.png");\n'
        self.assertTrue(
            gate.should_publish(
                {"src-tauri/src/tray.rs": raw, "src-tauri/icons/128x128.png": b"old"},
                {"src-tauri/src/tray.rs": raw, "src-tauri/icons/128x128.png": b"new"},
            )
        )

    def test_test_only_include_of_fixture_does_not_publish(self) -> None:
        src = '''
fn shipped() {}
#[cfg(test)]
mod tests {
    const X: &[u8] = include_bytes!("fixtures/sample.bin");
}
'''
        before = {
            "crates/app/src/lib.rs": src,
            "crates/app/src/fixtures/sample.bin": b"old",
        }
        after = {
            "crates/app/src/lib.rs": src,
            "crates/app/src/fixtures/sample.bin": b"new",
        }
        self.assertFalse(gate.should_publish(before, after))

    def test_unresolved_include_publishes(self) -> None:
        src = '''
const X: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/guide.md"));
'''
        self.assertTrue(gate.should_publish({"src/lib.rs": src}, {"src/lib.rs": src}))
        stripped = '''
fn shipped() {}
#[cfg(test)]
mod tests {
    const X: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/x"));
}
'''
        self.assertFalse(
            gate.should_publish(
                {"src/lib.rs": stripped, "docs/guide.md": "old"},
                {"src/lib.rs": stripped, "docs/guide.md": "new"},
            )
        )

    def test_include_macro_and_rust_fixtures(self) -> None:
        parent = 'include!("../docs/snippet.rs");\n'
        self.assertEqual(
            gate.shippable_includes({"src/lib.rs": parent}),
            {"docs/snippet.rs"},
        )
        before = {"src/lib.rs": parent, "docs/snippet.rs": "fn other() { 1 }\n"}
        after = {"src/lib.rs": parent, "docs/snippet.rs": "fn other() { 2 }\n"}
        self.assertTrue(gate.should_publish(before, after))
        unresolved = 'include!(concat!("fixtures/helper.rs"));\n'
        self.assertTrue(
            gate.should_publish({"src/lib.rs": unresolved}, {"src/lib.rs": unresolved})
        )
        fixture = '''
fn shipped() { 1 }
#[cfg(test)]
mod tests { fn scratch() { let _ = "helper"; } }
'''
        path = "crates/app/src/fixtures/helper.rs"
        self.assertFalse(
            gate.should_publish({path: fixture}, {path: fixture.replace("helper", "changed")})
        )
        self.assertTrue(
            gate.should_publish(
                {path: fixture},
                {path: fixture.replace("fn shipped() { 1 }", "fn shipped() { 2 }")},
            )
        )

    def test_real_shippable_includes_resolve(self) -> None:
        files: dict[str, str | None] = {}
        for path in ROOT.rglob("*.rs"):
            if "target" in path.parts:
                continue
            files[str(path.relative_to(ROOT))] = path.read_text(encoding="utf-8")
        included = gate.shippable_includes(files)
        self.assertIn("src-tauri/icons/128x128.png", included)
        self.assertNotIn("crates/seam-core/src/link/fixtures/rcgen-0.13.2-cert.der", included)
        self.assertFalse(gate.should_publish(files, dict(files)))


class GitDecisionTests(unittest.TestCase):
    def test_tag_comparison_and_manual_dispatch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self._git(tmp, ["init"])
            self._git(tmp, ["config", "user.email", "gate@example.com"])
            self._git(tmp, ["config", "user.name", "gate"])
            lib = Path(tmp) / "src"
            lib.mkdir()
            (lib / "lib.rs").write_text(PROD, encoding="utf-8")
            (Path(tmp) / "README.md").write_text("docs\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "base"])
            self._git(tmp, ["tag", "v0.1.9"])
            # An older-looking tag created later must not win version sort.
            self._git(tmp, ["tag", "v0.1.2"])
            (lib / "lib.rs").write_text(
                PROD.replace("seam-file-srv", "edited"), encoding="utf-8"
            )
            (Path(tmp) / "README.md").write_text("docs changed\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "tests and docs"])
            self._git(tmp, ["tag", "v0.1.10"])
            (lib / "lib.rs").write_text(
                PROD.replace("seam-file-srv", "edited-again"), encoding="utf-8"
            )
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "still only tests"])
            releases = {"v0.1.2", "v0.1.9", "v0.1.10"}
            self.assertEqual(gate.latest_release_tag(tmp, releases=releases), "v0.1.10")
            self.assertFalse(gate.decide_git(cwd=tmp, releases=releases))
            (lib / "lib.rs").write_text(
                PROD.replace("fn shipped() -> i32 { 1 }", "fn shipped() -> i32 { 9 }"),
                encoding="utf-8",
            )
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "real code"])
            self.assertTrue(gate.decide_git(cwd=tmp, releases=releases))
            env = os.environ.copy()
            env["GITHUB_EVENT_NAME"] = "workflow_dispatch"
            env.pop("GITHUB_OUTPUT", None)
            proc = subprocess.run(
                ["python3", str(ROOT / "scripts/publish_gate.py")],
                cwd=tmp,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("publish=true", proc.stdout)

    def test_no_tag_publishes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self._git(tmp, ["init"])
            self._git(tmp, ["config", "user.email", "gate@example.com"])
            self._git(tmp, ["config", "user.name", "gate"])
            (Path(tmp) / "README.md").write_text("only docs\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "docs"])
            self.assertTrue(gate.decide_git(cwd=tmp))

    def test_newest_merged_commit_beats_a_higher_version(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self._init(tmp)
            lib = Path(tmp) / "src"
            lib.mkdir()
            (lib / "lib.rs").write_text("fn shipped() { 1 }\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "base"])
            self._git(tmp, ["tag", "v9.9.9"])
            (lib / "lib.rs").write_text("fn shipped() { 2 }\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "real"])
            self._git(tmp, ["tag", "v0.1.1"])
            releases = {"v9.9.9", "v0.1.1"}
            # v9.9.9 is a higher version on an older commit. The tag on the
            # newer commit is the baseline, so this HEAD does not publish.
            self.assertEqual(gate.latest_release_tag(tmp, releases=releases), "v0.1.1")
            self.assertFalse(gate.decide_git(cwd=tmp, releases=releases))

    def test_unmerged_tag_is_ignored(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self._init(tmp)
            (Path(tmp) / "README.md").write_text("docs\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "base"])
            self._git(tmp, ["tag", "v0.1.1"])
            branch = subprocess.check_output(
                ["git", "branch", "--show-current"], cwd=tmp, text=True
            ).strip()
            self._git(tmp, ["checkout", "-b", "side"])
            (Path(tmp) / "src").mkdir()
            (Path(tmp) / "src" / "lib.rs").write_text("fn shipped() { 9 }\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "side"])
            self._git(tmp, ["tag", "v8.0.0"])
            self._git(tmp, ["checkout", branch])
            (Path(tmp) / "README.md").write_text("docs changed\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "docs"])
            releases = {"v0.1.1", "v8.0.0"}
            self.assertEqual(gate.latest_release_tag(tmp, releases=releases), "v0.1.1")
            self.assertFalse(gate.decide_git(cwd=tmp, releases=releases))

    def test_missing_github_release_publishes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self._init(tmp)
            (Path(tmp) / "README.md").write_text("docs\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "base"])
            self._git(tmp, ["tag", "v0.1.1"])
            (Path(tmp) / "README.md").write_text("docs changed\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "docs"])
            with self.assertRaises(gate.Uncertain):
                gate.latest_release_tag(tmp, releases=set())
            # Docs-only would skip if the tag counted. No release → publish.
            self.assertTrue(gate.decide_git(cwd=tmp, releases=set()))
            self.assertFalse(gate.decide_git(cwd=tmp, releases={"v0.1.1"}))
            original = gate._github_release_tags

            def boom(_cwd: str | None) -> set[str]:
                raise gate.Uncertain("gh failed")

            gate._github_release_tags = boom
            try:
                self.assertTrue(gate.decide_git(cwd=tmp))
            finally:
                gate._github_release_tags = original

    def test_chmod_submodule_and_non_utf8_publish(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self._init(tmp)
            lib = Path(tmp) / "src"
            lib.mkdir()
            (lib / "lib.rs").write_text("fn shipped() { 1 }\n", encoding="utf-8")
            (Path(tmp) / "README.md").write_text("docs\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "base"])
            self._git(tmp, ["tag", "v0.1.1"])
            releases = {"v0.1.1"}
            self._git(tmp, ["update-index", "--chmod=+x", "README.md"])
            self._git(tmp, ["commit", "-m", "chmod readme"])
            self.assertFalse(gate.decide_git(cwd=tmp, releases=releases))
            self._git(tmp, ["update-index", "--chmod=+x", "src/lib.rs"])
            self._git(tmp, ["commit", "-m", "chmod source"])
            self.assertTrue(gate.decide_git(cwd=tmp, releases=releases))

        with tempfile.TemporaryDirectory() as tmp:
            self._init(tmp)
            (Path(tmp) / "README.md").write_text("docs\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "base"])
            self._git(tmp, ["tag", "v0.1.1"])
            sub = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=tmp).decode().strip()
            payload = f"160000 {sub} 0\tvendor/lib\0".encode()
            subprocess.run(
                ["git", "update-index", "-z", "--index-info"],
                cwd=tmp,
                input=payload,
                check=True,
            )
            self._git(tmp, ["commit", "-m", "submodule"])
            self.assertTrue(gate.decide_git(cwd=tmp, releases={"v0.1.1"}))

        with tempfile.TemporaryDirectory() as tmp:
            self._init(tmp)
            (Path(tmp) / "README.md").write_text("docs\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "base"])
            self._git(tmp, ["tag", "v0.1.1"])
            blob = subprocess.check_output(
                ["git", "hash-object", "-w", "--stdin"], cwd=tmp, input=b"data"
            ).decode().strip()
            payload = f"100644 {blob} 0\t".encode() + b"weird/\xff.bin\0"
            subprocess.run(
                ["git", "update-index", "-z", "--index-info"],
                cwd=tmp,
                input=payload,
                check=True,
            )
            self._git(tmp, ["commit", "-m", "odd name"])
            self.assertTrue(gate.decide_git(cwd=tmp, releases={"v0.1.1"}))

    def _init(self, tmp: str) -> None:
        self._git(tmp, ["init", "-b", "main"])
        self._git(tmp, ["config", "user.email", "gate@example.com"])
        self._git(tmp, ["config", "user.name", "gate"])

    @staticmethod
    def _git(cwd: str, args: list[str]) -> None:
        subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True)


class ReleaseWorkflowTests(unittest.TestCase):
    def test_gate_job_and_skip_notification(self) -> None:
        text = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        gate = _job(text, "gate")
        build = _job(text, "build")
        publish = _job(text, "publish")
        self.assertIn("contents: read", gate)
        self.assertNotIn("contents: write", gate)
        self.assertIn("timeout-minutes: 5", gate)
        self.assertIn("persist-credentials: false", gate)
        self.assertEqual(text.count("persist-credentials:"), 1)
        self.assertIn("needs: [plan, gate]", build)
        self.assertIn("needs.gate.result != 'success'", build)
        self.assertIn("needs.gate.outputs.publish != 'false'", build)
        self.assertIn("needs.plan.result == 'success'", build)
        self.assertIn("no release was made", text.split("jobs:", 1)[0])
        self.assertIn("needs.gate.outputs.publish != 'false'", publish)
        self.assertNotIn("needs.gate.outputs.publish == 'true'", publish)
        self.assertNotIn("needs.build.result", text)
        self.assertNotIn("pull_request:", text)
        decide = _step(gate, "Decide whether this commit ships app code")
        self.assertIn("GH_TOKEN: ${{ github.token }}", decide)
        self.assertIn("\n        env:\n", "\n" + decide + "\n")
        self.assertNotIn("GH_TOKEN", _step(gate, "Fetch release tags"))


class ReleaseListTests(unittest.TestCase):
    def test_release_list_excludes_drafts(self) -> None:
        captured: dict[str, list[str]] = {}

        def fake_run(args, **_kwargs):
            captured["args"] = list(args)

            class Proc:
                returncode = 0
                stdout = b"[]"
                stderr = b""

            return Proc()

        original = gate.subprocess.run
        gate.subprocess.run = fake_run
        try:
            self.assertEqual(gate._github_release_tags(None), set())
        finally:
            gate.subprocess.run = original
        self.assertEqual(
            captured["args"],
            [
                "gh",
                "release",
                "list",
                "--exclude-drafts",
                "--limit",
                "500",
                "--json",
                "tagName",
            ],
        )

    def test_release_list_at_the_page_limit_is_uncertain(self) -> None:
        payload = json.dumps([{"tagName": f"v0.0.{i}"} for i in range(500)]).encode()

        def fake_run(_args, **_kwargs):
            class Proc:
                returncode = 0
                stdout = payload
                stderr = b""

            return Proc()

        original = gate.subprocess.run
        gate.subprocess.run = fake_run
        try:
            with self.assertRaises(gate.Uncertain) as raised:
                gate._github_release_tags(None)
        finally:
            gate.subprocess.run = original
        self.assertIn("truncated", str(raised.exception))


def _step(job: str, name: str) -> str:
    lines = job.splitlines()
    start = None
    for index, line in enumerate(lines):
        if line.strip() == f"- name: {name}":
            start = index
            break
    if start is None:
        raise AssertionError(f"step {name} not found")
    block = [lines[start]]
    for line in lines[start + 1 :]:
        if line.startswith("      - "):
            break
        block.append(line)
    return "\n".join(block)


def _job(text: str, name: str) -> str:
    lines = text.splitlines()
    start = None
    for index, line in enumerate(lines):
        if line == f"  {name}:":
            start = index
            break
    if start is None:
        raise AssertionError(f"job {name} not found")
    block = [lines[start]]
    for line in lines[start + 1 :]:
        if line.startswith("  ") and not line.startswith("   ") and line.endswith(":"):
            break
        block.append(line)
    return "\n".join(block)


if __name__ == "__main__":
    unittest.main()
