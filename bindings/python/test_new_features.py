import os
import sys
from pathlib import Path

# Setup lib path
here = Path(__file__).resolve().parent
lib_path = here.parent.parent / "target" / "release" / "cendb_ffi.dll"
if not lib_path.exists():
    for ext in [".dll", ".so", ".dylib"]:
        candidate = here.parent.parent / "target" / "release" / f"cendb_ffi{ext}"
        if candidate.exists():
            lib_path = candidate
            break

os.environ["CENDB_LIB_PATH"] = str(lib_path)
sys.path.append(str(here))

import cendb

def test_graph():
    print("Testing Graph database FFI...")
    db = cendb.open()
    try:
        # Construct graph
        db.graph_add_node(0, "User")
        db.graph_add_node(1, "User")
        db.graph_add_node(2, "Product")
        
        db.graph_add_edge(0, 1, "follows")
        db.graph_add_edge(1, 2, "bought")
        
        # Traverse graph (BFS start_node=0, depth=2)
        visited = db.graph_bfs(0, 2)
        print("BFS visited nodes:", visited)
        assert len(visited) == 3, f"Expected 3 nodes, got {len(visited)}"
        assert visited[0] == 0, f"Expected node 0, got {visited[0]}"
        print("Graph database FFI test passed successfully!")
    finally:
        db.close()

def test_document():
    print("\nTesting Document database FFI...")
    db = cendb.open()
    try:
        mock_doc = bytes([72, 69, 88, 68, 6, 0, 0, 0, 3, 0, 0, 0, 20, 0, 0, 0, 65, 0, 0, 0, 6, 3, 0, 0, 0, 0, 0, 0, 0, 49, 0, 0, 0, 17, 0, 0, 0, 54, 0, 0, 0, 24, 0, 0, 0, 63, 0, 0, 0, 4, 8, 0, 0, 0, 2, 30, 0, 0, 0, 0, 0, 0, 0, 1, 1, 4, 0, 0, 0, 110, 97, 109, 101, 5, 0, 0, 0, 65, 108, 105, 99, 101, 3, 0, 0, 0, 97, 103, 101, 6, 0, 0, 0, 97, 99, 116, 105, 118, 101])
        db.doc_put(b"user_doc", mock_doc)
        
        name_val = db.doc_get_field(b"user_doc", "name")
        print("Queried nested field 'name':", name_val)
        assert "Alice" in name_val, f"Expected Alice, got {name_val}"
        print("Document database FFI test passed successfully!")
    finally:
        db.close()

if __name__ == "__main__":
    test_graph()
    test_document()
