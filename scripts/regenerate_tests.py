#!/usr/bin/env python3
"""Auto-generate tree-sitter test expectations from actual parser output."""
import subprocess
import re

def parse_rsc(code):
    """Parse a single RSC code block and return the tree string."""
    import tempfile, os
    with tempfile.NamedTemporaryFile(mode='w', suffix='.rsc', delete=False) as f:
        f.write(code)
        tmp = f.name
    
    cmd = ['tree-sitter', 'parse', tmp]
    result = subprocess.run(cmd, capture_output=True, text=True, cwd='grammars/rsc')
    os.unlink(tmp)
    
    # Extract just the parse tree, skip warnings
    lines = result.stdout.split('\n')
    tree_lines = []
    in_tree = False
    for line in lines:
        if line.startswith('(source_file'):
            in_tree = True
        if in_tree:
            tree_lines.append(line)
    return '\n'.join(tree_lines)

def process_corpus_file(filepath):
    """Process a corpus file and rewrite expected outputs."""
    with open(filepath, 'r') as f:
        content = f.read()
    
    # Split into test cases
    parts = re.split(r'(^={10,}\n)', content, flags=re.MULTILINE)
    
    new_content = []
    i = 0
    while i < len(parts):
        if parts[i].startswith('=========='):
            # Test case separator + name
            header = parts[i] + (parts[i+1] if i+1 < len(parts) and not parts[i+1].startswith('=') else '')
            i += 1
            if i < len(parts) and not parts[i].startswith('='):
                i += 1
            
            # Get the input code (up to ---)
            input_code = ''
            if i < len(parts):
                input_code = parts[i]
                i += 1
            
            # Skip old expected output (after ---)
            if i < len(parts) and parts[i].strip() == '---':
                i += 1
                # Skip the old tree
                while i < len(parts) and not parts[i].startswith('==========') and parts[i].strip() != '':
                    i += 1
            
            # Parse and generate new expected output
            code_to_parse = input_code.strip()
            if code_to_parse:
                tree = parse_rsc(code_to_parse)
            else:
                tree = parse_rsc('\n')
            
            new_content.append(header)
            new_content.append(code_to_parse)
            new_content.append('\n---\n')
            new_content.append(tree + '\n')
        else:
            i += 1
    
    with open(filepath, 'w') as f:
        f.write(''.join(new_content))
    print(f'Updated: {filepath}')

if __name__ == '__main__':
    from pathlib import Path
    corpus_dir = Path('grammars/rsc/test/corpus')
    for f in sorted(corpus_dir.glob('*.txt')):
        process_corpus_file(str(f))
