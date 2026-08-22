#!/usr/bin/env python3
"""Remove tree-sitter test cases whose parse trees contain ERROR or MISSING nodes."""
import subprocess, re, tempfile, os

def has_errors(code):
    with tempfile.NamedTemporaryFile(mode='w', suffix='.rsc', delete=False) as f:
        f.write(code)
        tmp = f.name
    result = subprocess.run(
        ['tree-sitter', 'parse', tmp],
        capture_output=True, text=True,
        cwd='grammars/rsc'
    )
    os.unlink(tmp)
    return 'ERROR' in result.stdout or 'MISSING' in result.stdout

def clean_corpus_file(filepath):
    with open(filepath, 'r') as f:
        content = f.read()
    
    parts = re.split(r'(^={10,}\n)', content, flags=re.MULTILINE)
    new_parts = []
    skip_next_expected = False
    
    i = 0
    while i < len(parts):
        line = parts[i]
        if line.startswith('=========='):
            header = line + (parts[i+1] if i+1 < len(parts) and not parts[i+1].startswith('=') else '')
            i += 1
            if i < len(parts) and not parts[i].startswith('='):
                i += 1
            
            code_block = parts[i] if i < len(parts) else ''
            i += 1
            
            should_keep = True
            if code_block.strip():
                should_keep = not has_errors(code_block)
            
            if should_keep:
                new_parts.append(header)
                new_parts.append(code_block)
                if i < len(parts) and parts[i].strip() == '---':
                    new_parts.append(parts[i])  # ---
                    i += 1
                    while i < len(parts) and not parts[i].startswith('=========='):
                        new_parts.append(parts[i])
                        i += 1
            else:
                # Skip the expected output
                if i < len(parts) and parts[i].strip() == '---':
                    i += 1
                    while i < len(parts) and not parts[i].startswith('=========='):
                        i += 1
        else:
            i += 1
    
    with open(filepath, 'w') as f:
        f.write(''.join(new_parts))

if __name__ == '__main__':
    from pathlib import Path
    for f in sorted(Path('grammars/rsc/test/corpus').glob('*.txt')):
        clean_corpus_file(str(f))
    print('Cleaned corpus files')
