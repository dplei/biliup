"""Run with: python3 -m unittest discover -s scripts -p 'test_check_code_index.py'."""

import tempfile
import unittest
from pathlib import Path

from check_code_index import validate_index


class CodeIndexTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.write('CODE_INDEX.md', '[A](docs/code-index/a.md)\n[B](docs/code-index/b.md)\n')
        self.write('docs/code-index/a.md', '| `a.py` | entry | `run` |\n- `a.py` → `b.py`（`run`）：call\n')
        self.write('docs/code-index/b.md', '| `b.py` | worker | `work` |\n')
        self.write('a.py', '')
        self.write('b.py', '')

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding='utf-8')

    def errors(self):
        return '\n'.join(validate_index(self.root)[0])

    def test_cross_document_relationship(self):
        self.assertEqual(validate_index(self.root), ([], 2, 1))

    def test_duplicate_entry_and_relation_across_documents(self):
        self.write('docs/code-index/b.md', '| `a.py` | duplicate | `run` |\n- `a.py` → `b.py`（`run`）：call\n')
        errors = self.errors()
        self.assertIn('duplicate file entry', errors)
        self.assertIn('duplicate relationship', errors)
        self.assertIn('endpoint has no file entry: b.py', errors)

    def test_all_endpoints_in_grouped_and_chained_relations(self):
        self.write('docs/code-index/a.md', '| `a.py` | entry | `run` |\n- `a.py`、`missing.py` → `b.py` → `last.py`（`run`）：call\n')
        errors = self.errors()
        self.assertIn('file does not exist: missing.py', errors)
        self.assertIn('endpoint has no file entry: last.py', errors)

    def test_broken_and_missing_navigation_links(self):
        self.write('CODE_INDEX.md', '[A](docs/code-index/a.md)\n[Missing](docs/code-index/missing.md)\n')
        errors = self.errors()
        self.assertIn('file does not exist: docs/code-index/missing.md', errors)
        self.assertIn('docs/code-index/b.md: missing link from CODE_INDEX.md', errors)

    def test_relative_links_and_path_escape(self):
        self.write('docs/code-index/a.md', '[Root](../../CODE_INDEX.md)\n[Escape](../../../outside.md)\n| `../outside.py` | bad | — |\n| `/absolute.py` | bad | — |\n')
        errors = self.errors()
        self.assertIn('path escapes the repository', errors)
        self.assertIn('path must be relative', errors)
        self.assertNotIn('file does not exist: docs/code-index/../../CODE_INDEX.md', errors)

    def test_missing_root_and_empty_index(self):
        (self.root / 'CODE_INDEX.md').unlink()
        self.assertIn('file does not exist: CODE_INDEX.md', self.errors())
        self.write('docs/code-index/a.md', '')
        self.write('docs/code-index/b.md', '')
        self.assertIn('contains no file entries', self.errors())

    def test_symlink_escape(self):
        with tempfile.TemporaryDirectory() as outside:
            target = Path(outside) / 'outside.py'
            target.write_text('')
            (self.root / 'a.py').unlink()
            (self.root / 'a.py').symlink_to(target)
            self.assertIn('path escapes the repository: a.py', self.errors())


if __name__ == '__main__':
    unittest.main()
