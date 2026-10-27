"""Extract a named client class from reviewed external source without its globals.

This is an isolation helper for trusted tests, not a sandbox for hostile code.
"""
import ast
from pathlib import Path
import time


class UpstreamError(Exception):
    pass


def client_type(reference, http_json, class_name="ExternalClient"):
    tree = ast.parse(Path(reference).read_text())
    selected = [node for node in tree.body
                if isinstance(node, ast.ClassDef) and node.name == class_name]
    if len(selected) != 1:
        raise ValueError('reference client class absent or ambiguous')
    namespace = {'time': time, 'http_json': http_json, 'UpstreamError': UpstreamError}
    exec(compile(ast.Module(body=selected, type_ignores=[]), '<reference-client>', 'exec'),
         namespace)
    return namespace[class_name]
