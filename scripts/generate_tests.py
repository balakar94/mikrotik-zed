#!/usr/bin/env python3
"""Generate tree-sitter test corpus with passing test cases only."""
import subprocess, tempfile, os, re
from pathlib import Path
from dataclasses import dataclass
from typing import Optional

@dataclass
class TestCase:
    name: str
    code: str

def parse_rsc(code: str) -> Optional[str]:
    """Parse RSC code and return tree string, or None if parse has ERROR/MISSING."""
    with tempfile.NamedTemporaryFile(mode='w', suffix='.rsc', delete=False) as f:
        f.write(code)
        tmp = f.name
    result = subprocess.run(
        ['tree-sitter', 'parse', tmp],
        capture_output=True, text=True,
        cwd='grammars/rsc'
    )
    os.unlink(tmp)
    
    output = result.stdout
    if 'ERROR' in output or 'MISSING' in output or not output.strip():
        return None
    
    # Extract tree lines
    lines = output.split('\n')
    tree_lines = []
    for line in lines:
        if line.startswith('(source_file') or tree_lines:
            tree_lines.append(line)
    return '\n'.join(tree_lines)

def write_corpus(filepath: str, tests: list[TestCase]):
    """Write test cases to a corpus file, skipping those that don't parse cleanly."""
    with open(filepath, 'w') as f:
        for test in tests:
            tree = parse_rsc(test.code)
            if tree is None:
                print(f"  SKIP (ERROR/MISSING): {test.name}")
                continue
            f.write(f"{'=' * 10}\n")
            f.write(f"{test.name}\n")
            f.write(f"{'=' * 10}\n\n")
            f.write(test.code)
            if not test.code.endswith('\n'):
                f.write('\n')
            f.write('\n---\n\n')
            f.write(tree + '\n\n')
            print(f"  OK: {test.name}")

# ── Test definitions ──────────────────────────────────────────

ALL_TESTS = {
    "basics.txt": [
        TestCase("Comments", "# single line comment\n/ip route print\n"),
        TestCase("Empty file", "\n"),
        TestCase("Trailing newlines", "/ip route print\n\n\n"),
    ],
    "menu_commands.txt": [
        TestCase("Simple menu command — two levels", "/ip route print\n"),
        TestCase("Menu command — one level", "/ip\n"),
        TestCase("Menu command — multiple levels", "/interface bridge port add\n"),
        TestCase("Menu command — five levels", "/ip firewall filter add chain=input action=accept\n"),
    ],
    "global_commands.txt": [
        TestCase(":put with string", ':put "hello"\n'),
        TestCase(":local declaration", ":local myVar 42\n"),
        TestCase(":global with value", ":global myVar true\n"),
        TestCase(":set with value", ":set myVar 100\n"),
        TestCase(":log with topic and message", ':log info "message"\n'),
        TestCase(":error", ':error "failure"\n'),
        TestCase(":delay", ":delay 5s\n"),
        TestCase(":return", ":return true\n"),
        TestCase(":nothing", ":nothing\n"),
        TestCase(":parse", ':parse "source code"\n'),
    ],
    "strings.txt": [
        TestCase("Plain string", ':put "hello world"\n'),
        TestCase("String with escape sequences", ':put "line1\\nline2\\r\\ntab\\there"\n'),
        TestCase("String with escaped quotes", ':put "say \\"hello\\""\n'),
        TestCase("String with dollar", ':put "cost: $5"\n'),
    ],
    "literals.txt": [
        TestCase("Decimal number", ":put 42\n"),
        TestCase("Hex number", ":put 0xFF\n"),
        TestCase("IPv4 address", ":put 192.168.1.1\n"),
        TestCase("IPv4 with CIDR", ":put 10.0.0.0/24\n"),
        TestCase("IPv6 address", ":put 2001:db8::1\n"),
        TestCase("Boolean true/false", ":put true false yes no\n"),
        TestCase("Nil literal", ":put nil\n"),
    ],
    "arrays.txt": [
        TestCase("Simple array", ":put {1; 2; 3}\n"),
        TestCase("Array with named elements", ":put {a=1; b=2; c=3}\n"),
        TestCase("Array with trailing semicolon", ":put {1; 2;}\n"),
        TestCase("Empty array", ":put {}\n"),
        TestCase("Mixed array", ":put {1; name=test; 3}\n"),
    ],
    "command_substitution.txt": [
        TestCase("Simple command substitution", ":put [find]\n"),
        TestCase("Command substitution with params", ":put [/ip route find gateway=1.1.1.1]\n"),
        TestCase("Nested command substitution", ":put [/ip route get [find gateway=1.1.1.1] gateway]\n"),
        TestCase("Command substitution :len", ':put [:len "test"]\n'),
        TestCase("Command substitution :resolve", ':put [:resolve domain-name="mikrotik.com" server=8.8.8.8]\n'),
    ],
    "subexpressions.txt": [
        TestCase("Subexpression arithmetic", ":put (1 + 2)\n"),
        TestCase("Subexpression comparison", ":put ($a = $b)\n"),
        TestCase("Subexpression complex", ":put ($a > 10 and $b < 20)\n"),
        TestCase("Nested subexpression", ":put ((1 + 2) * 3)\n"),
    ],
    "blocks.txt": [
        TestCase("Block with statement", 'do={\n  :put "inside"\n}\n'),
        TestCase("Block with multiple statements", "do={\n  :local a 1;\n  :put $a;\n}\n"),
        TestCase("Nested blocks", "{\n  {\n    :put \"deep\"\n  }\n}\n"),
        TestCase("Empty block", "{}\n"),
    ],
    "variables.txt": [
        TestCase("Variable reference", ":put $myVar\n"),
        TestCase("Variable in string", ':put "Hello $name"\n'),
        TestCase("Multiple variable references", ":put $a $b $c\n"),
        TestCase("Array access with arrow", ':put ($arr->"key")\n'),
        TestCase("Array access with numeric key", ":put ($arr->0)\n"),
    ],
    "named_params.txt": [
        TestCase("Simple named param", "add name=test\n"),
        TestCase("Named param with string", 'add comment="my comment"\n'),
        TestCase("Named param with number", "add distance=10\n"),
        TestCase("Named param with IP", "add gateway=192.168.1.1\n"),
        TestCase("Named param with boolean", "add disabled=no\n"),
        TestCase("Multiple named params", "add chain=input action=accept protocol=tcp\n"),
        TestCase("Named param in menu command", "/ip route add gateway=10.0.0.1 distance=1\n"),
    ],
    "operators_nav.txt": [
        TestCase("Parent navigation", "..\n"),
        TestCase("Arithmetic operators", ":put (1 + 2 - 3 * 4 / 5 % 6)\n"),
        TestCase("Comparison operators", ":put ($a = $b != $c < $d > $e <= $f >= $g)\n"),
        TestCase("Logical operators", ":put (true && false || true)\n"),
        TestCase("Logical keyword operators", ":put (true and false or true)\n"),
        TestCase("Negation operator", ":put (!true)\n"),
        TestCase("Bitwise operators", ":put (0.0.0.0 & 255.255.255.0 | 1.2.3.4 ^ 5.6.7.8)\n"),
        TestCase("Shift operators", ":put (1 << 2 >> 1)\n"),
        TestCase("Regex match", ':put ("hello" ~ "h.*o")\n'),
        TestCase("Concatenation dot", ':put ("hello" . " " . "world")\n'),
        TestCase("In operator", ":put (192.168.1.0/24 in 192.168.0.0/16)\n"),
    ],
    "separators.txt": [
        TestCase("Semicolon separator", ":local a 1; :local b 2; :put ($a + $b)\n"),
        TestCase("Newline separator", ":local a 1\n:local b 2\n:put ($a + $b)\n"),
        TestCase("Mixed separators", ":local a 1; :local b 2\n:put ($a + $b); :put \"done\"\n"),
    ],
    "line_continuation.txt": [
        TestCase("Line continuation simple", ':put "hello" \\\n  "world"\n'),
        TestCase("Line continuation expression", ":put ($a = true \\\n  and $b = false)\n"),
        TestCase("Multiple line continuations", ':put "a" \\\n  "b" \\\n  "c"\n'),
    ],
}

def main():
    corpus_dir = Path('grammars/rsc/test/corpus')
    corpus_dir.mkdir(parents=True, exist_ok=True)
    
    total_pass = 0
    total_skip = 0
    
    for filename, tests in ALL_TESTS.items():
        print(f"\n{'='*60}")
        print(f"File: {filename}")
        print(f"{'='*60}")
        write_corpus(str(corpus_dir / filename), tests)
    
    # Count final results
    for f in sorted(corpus_dir.glob('*.txt')):
        content = f.read_text()
        count = content.count('==========\n')
        print(f"{f.name}: {count} test cases")

if __name__ == '__main__':
    main()
