#!/usr/bin/env python3
"""ForgeCode databases module: one engine for every SQL DBMS.

  python db.py serve         JSON lines on stdin/stdout for the editor ({"id", "method", "params"})
  python db.py mcp FILE      MCP server (stdio) for agents such as Hermes; FILE lists connections

Drivers: sqlite3 (built in), psycopg (PostgreSQL), pymysql (MySQL / MariaDB), pymssql (SQL
Server), oracledb (Oracle, thin mode: no Oracle client needed). Missing drivers are reported
when a connection needs them.

Python 3.9 compatible (tests run with the system Python).
"""

import csv
import datetime
import decimal
import json
import os
import re
import sys
import urllib.request
import uuid

KINDS = ("sqlite", "postgresql", "mysql", "mariadb", "sqlserver", "oracle")
DEFAULT_PORTS = {"postgresql": 5432, "mysql": 3306, "mariadb": 3306, "sqlserver": 1433, "oracle": 1521}
READ_KEYWORDS = ("select", "with", "show", "explain", "describe", "desc", "values", "pragma", "table")
MAX_ROWS = 1000


class DbError(Exception):
    pass


# ---------------------------------------------------------------- connections


def connect(cfg):
    kind = cfg.get("kind", "sqlite")
    if kind not in KINDS:
        raise DbError("Unknown database kind: %s" % kind)
    try:
        if kind == "sqlite":
            import sqlite3

            path = cfg.get("path") or ""
            if not path:
                raise DbError("No SQLite file")
            if cfg.get("readonly"):
                conn = sqlite3.connect("file:%s?mode=ro" % path, uri=True)
            else:
                conn = sqlite3.connect(path)
            return conn
        port = int(cfg.get("port") or DEFAULT_PORTS[kind])
        host, user, password, database = cfg.get("host") or "localhost", cfg.get("user") or "", cfg.get("password") or "", cfg.get("database") or ""
        if kind == "postgresql":
            import psycopg

            conn = psycopg.connect(host=host, port=port, user=user, password=password, dbname=database or None, connect_timeout=10)
            if cfg.get("readonly"):
                conn.read_only = True
            return conn
        if kind in ("mysql", "mariadb"):
            import pymysql

            return pymysql.connect(host=host, port=port, user=user, password=password, database=database or None, connect_timeout=10, charset="utf8mb4")
        if kind == "sqlserver":
            import pymssql

            return pymssql.connect(server=host, port=port, user=user, password=password, database=database or None, login_timeout=10)
        if kind == "oracle":
            import oracledb

            return oracledb.connect(user=user, password=password, dsn="%s:%s/%s" % (host, port, database))
    except ImportError as e:
        raise DbError("The driver for %s is not installed (%s)" % (kind, e.name))
    except DbError:
        raise
    except Exception as e:
        raise DbError(str(e).strip() or type(e).__name__)


def placeholder(kind, i):
    """The driver's parameter marker for the i-th (0-based) parameter."""
    if kind == "sqlite":
        return "?"
    if kind == "oracle":
        return ":%d" % (i + 1)
    return "%s"


def quote(kind, name):
    if kind in ("mysql", "mariadb"):
        return "`%s`" % name.replace("`", "``")
    if kind == "sqlserver":
        return "[%s]" % name.replace("]", "]]")
    return '"%s"' % name.replace('"', '""')


def qualified(kind, table):
    """`schema.table` quoted part by part."""
    return ".".join(quote(kind, p) for p in table.split("."))


def is_write(sql):
    """Whether a statement changes data or structure (comments ignored)."""
    text = re.sub(r"--[^\n]*|/\*.*?\*/", " ", sql, flags=re.S).strip().lower()
    statements = [s.strip() for s in text.split(";") if s.strip()]
    for s in statements:
        first = s.split(None, 1)[0] if s else ""
        if first not in READ_KEYWORDS:
            return True
        # WITH … DELETE/UPDATE/INSERT, PRAGMA x = y
        if first == "with" and re.search(r"\b(insert|update|delete|merge)\b", s):
            return True
        if first == "pragma" and "=" in s:
            return True
    return False


def safe(value):
    """A JSON-friendly cell."""
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, decimal.Decimal):
        return str(value)
    if isinstance(value, (datetime.date, datetime.datetime, datetime.time)):
        return value.isoformat()
    if isinstance(value, (bytes, bytearray, memoryview)):
        data = bytes(value)
        return "0x" + data[:64].hex() + ("…" if len(data) > 64 else "")
    if isinstance(value, uuid.UUID):
        return str(value)
    return str(value)


# ---------------------------------------------------------------- engine


class Engine:
    def __init__(self):
        self.connections = {}
        self.configs = {}

    def open(self, cid, cfg):
        old = self.connections.pop(cid, None)
        if old is not None:
            try:
                old.close()
            except Exception:
                pass
        conn = connect(cfg)
        self.connections[cid] = conn
        self.configs[cid] = cfg
        return {"ok": True, "version": self.version(cid)}

    def conn(self, cid):
        if cid not in self.connections:
            if cid in self.configs:
                self.connections[cid] = connect(self.configs[cid])
            else:
                raise DbError("Not connected")
        return self.connections[cid], self.configs[cid]["kind"]

    def close(self, cid):
        conn = self.connections.pop(cid, None)
        if conn is not None:
            conn.close()
        return {"ok": True}

    def version(self, cid):
        conn, kind = self.conn(cid)
        sql = {
            "sqlite": "select 'SQLite ' || sqlite_version()",
            "postgresql": "select version()",
            "mysql": "select concat('MySQL ', version())",
            "mariadb": "select concat('MariaDB ', version())",
            "sqlserver": "select @@version",
            "oracle": "select banner from v$version where rownum = 1",
        }[kind]
        try:
            cur = conn.cursor()
            cur.execute(sql)
            row = cur.fetchone()
            return str(row[0]).splitlines()[0] if row else kind
        except Exception:
            return kind

    def _run(self, cid, sql, params=(), fetch=True, limit=MAX_ROWS):
        conn, kind = self.conn(cid)
        cur = conn.cursor()
        try:
            cur.execute(sql, params) if params else cur.execute(sql)
            if cur.description is not None and fetch:
                columns = [d[0] for d in cur.description]
                rows = cur.fetchmany(limit + 1)
                truncated = len(rows) > limit
                return {"columns": columns, "rows": [[safe(v) for v in r] for r in rows[:limit]], "truncated": truncated}
            conn.commit()
            return {"rowcount": cur.rowcount}
        except Exception as e:
            try:
                conn.rollback()
            except Exception:
                pass
            raise DbError(str(e).strip() or type(e).__name__)

    def query(self, cid, sql, allow_write=False, limit=MAX_ROWS):
        """Run SQL. Writes need `allow_write` (the editor asks the user first)."""
        cfg = self.configs.get(cid, {})
        if is_write(sql):
            if cfg.get("readonly"):
                raise DbError("This connection is read-only")
            if not allow_write:
                return {"needs_confirmation": True}
        return self._run(cid, sql, limit=limit)

    def schema(self, cid):
        """Tables and views with their columns and primary key."""
        conn, kind = self.conn(cid)
        out = []
        if kind == "sqlite":
            for name, typ in self._run(cid, "select name, type from sqlite_master where type in ('table','view') and name not like 'sqlite_%' order by name")["rows"]:
                cols = self._run(cid, "pragma table_info(%s)" % quote(kind, name))["rows"]
                out.append({
                    "name": name, "kind": typ,
                    "columns": [{"name": c[1], "type": c[2], "nullable": not c[3], "default": c[4], "pk": bool(c[5])} for c in cols],
                })
            return out
        if kind == "oracle":
            tables = self._run(cid, "select table_name, 'table' from user_tables union all select view_name, 'view' from user_views order by 1")["rows"]
            for name, typ in tables:
                cols = self._run(cid, "select column_name, data_type, nullable, data_default from user_tab_columns where table_name = :1 order by column_id", (name,))["rows"]
                pks = {r[0] for r in self._run(cid, "select cc.column_name from user_constraints c join user_cons_columns cc on c.constraint_name = cc.constraint_name where c.table_name = :1 and c.constraint_type = 'P'", (name,))["rows"]}
                out.append({"name": name, "kind": typ, "columns": [{"name": c[0], "type": c[1], "nullable": c[2] == "Y", "default": c[3], "pk": c[0] in pks} for c in cols]})
            return out
        # information_schema: PostgreSQL, MySQL/MariaDB, SQL Server
        if kind == "postgresql":
            where = "table_schema not in ('pg_catalog', 'information_schema')"
        elif kind in ("mysql", "mariadb"):
            where = "table_schema = database()"
        else:
            where = "table_schema not in ('sys', 'INFORMATION_SCHEMA')"
        tables = self._run(cid, "select table_schema, table_name, table_type from information_schema.tables where %s order by table_schema, table_name" % where)["rows"]
        p = placeholder(kind, 0), placeholder(kind, 1)
        for schema, name, ttype in tables:
            cols = self._run(
                cid,
                "select column_name, data_type, is_nullable, column_default from information_schema.columns where table_schema = %s and table_name = %s order by ordinal_position" % p,
                (schema, name),
            )["rows"]
            pks = {
                r[0]
                for r in self._run(
                    cid,
                    "select k.column_name from information_schema.table_constraints t join information_schema.key_column_usage k "
                    "on t.constraint_name = k.constraint_name and t.table_schema = k.table_schema and t.table_name = k.table_name "
                    "where t.constraint_type = 'PRIMARY KEY' and t.table_schema = %s and t.table_name = %s" % p,
                    (schema, name),
                )["rows"]
            }
            default_schema = {"postgresql": "public", "sqlserver": "dbo"}.get(kind)
            full = name if kind in ("mysql", "mariadb") or schema == default_schema else "%s.%s" % (schema, name)
            out.append({
                "name": full, "kind": "view" if "VIEW" in ttype.upper() else "table",
                "columns": [{"name": c[0], "type": c[1], "nullable": c[2] == "YES", "default": c[3], "pk": c[0] in pks} for c in cols],
            })
        return out

    def rows(self, cid, table, limit=200, offset=0, where="", order=""):
        """A page of a table, with its total row count."""
        conn, kind = self.conn(cid)
        t = qualified(kind, table)
        clause = (" where " + where) if where.strip() else ""
        total = self._run(cid, "select count(*) from %s%s" % (t, clause))["rows"][0][0]
        order_by = (" order by " + order) if order.strip() else ""
        if kind == "sqlserver":
            order_by = order_by or " order by (select null)"
            sql = "select * from %s%s%s offset %d rows fetch next %d rows only" % (t, clause, order_by, offset, limit)
        elif kind == "oracle":
            sql = "select * from %s%s%s offset %d rows fetch next %d rows only" % (t, clause, order_by, offset, limit)
        else:
            sql = "select * from %s%s%s limit %d offset %d" % (t, clause, order_by, limit, offset)
        page = self._run(cid, sql, limit=limit)
        page["total"] = total
        return page

    def _where_keys(self, kind, keys, start):
        parts, params = [], []
        for i, (col, val) in enumerate(keys.items()):
            if val is None:
                parts.append("%s is null" % quote(kind, col))
            else:
                parts.append("%s = %s" % (quote(kind, col), placeholder(kind, start + len(params))))
                params.append(val)
        return " and ".join(parts), params

    def _writable(self, cid):
        if self.configs.get(cid, {}).get("readonly"):
            raise DbError("This connection is read-only")

    def update_cell(self, cid, table, keys, column, value):
        """Set one cell; `keys` identify the row (primary key, else every original value)."""
        self._writable(cid)
        conn, kind = self.conn(cid)
        where, params = self._where_keys(kind, keys, 1)
        sql = "update %s set %s = %s where %s" % (qualified(kind, table), quote(kind, column), placeholder(kind, 0), where)
        return self._run(cid, sql, [value] + params, fetch=False)

    def insert_row(self, cid, table, values):
        self._writable(cid)
        conn, kind = self.conn(cid)
        cols = list(values)
        if not cols:
            sql = "insert into %s default values" % qualified(kind, table)
            return self._run(cid, sql, fetch=False)
        sql = "insert into %s (%s) values (%s)" % (
            qualified(kind, table), ", ".join(quote(kind, c) for c in cols), ", ".join(placeholder(kind, i) for i in range(len(cols))),
        )
        return self._run(cid, sql, [values[c] for c in cols], fetch=False)

    def delete_rows(self, cid, table, rows):
        self._writable(cid)
        conn, kind = self.conn(cid)
        count = 0
        for keys in rows:
            where, params = self._where_keys(kind, keys, 0)
            count += self._run(cid, "delete from %s where %s" % (qualified(kind, table), where), params, fetch=False)["rowcount"]
        return {"rowcount": count}

    def export_csv(self, cid, sql, path):
        conn, kind = self.conn(cid)
        if is_write(sql):
            raise DbError("Only a query can be exported")
        cur = conn.cursor()
        cur.execute(sql)
        n = 0
        with open(path, "w", newline="", encoding="utf-8") as f:
            w = csv.writer(f)
            w.writerow([d[0] for d in cur.description])
            for row in cur:
                w.writerow([safe(v) for v in row])
                n += 1
        return {"rows": n, "path": path}


def create_table_sql(kind, table, columns):
    """CREATE TABLE for `columns` [{name, type, pk, nullable, default}]."""
    lines, pks = [], [c["name"] for c in columns if c.get("pk")]
    for c in columns:
        line = "%s %s" % (quote(kind, c["name"]), c.get("type") or "TEXT")
        if not c.get("nullable", True) or c.get("pk"):
            line += " NOT NULL"
        if c.get("default") not in (None, ""):
            line += " DEFAULT %s" % c["default"]
        lines.append("  " + line)
    if pks:
        lines.append("  PRIMARY KEY (%s)" % ", ".join(quote(kind, p) for p in pks))
    return "CREATE TABLE %s (\n%s\n);" % (qualified(kind, table), ",\n".join(lines))


def alter_sql(kind, table, action, column):
    """ALTER TABLE for one change: add, drop or rename a column."""
    t = qualified(kind, table)
    if action == "add":
        line = "%s %s" % (quote(kind, column["name"]), column.get("type") or "TEXT")
        if not column.get("nullable", True):
            line += " NOT NULL"
        if column.get("default") not in (None, ""):
            line += " DEFAULT %s" % column["default"]
        keyword = "ADD" if kind in ("sqlserver", "oracle") else "ADD COLUMN"
        return "ALTER TABLE %s %s %s;" % (t, keyword, line)
    if action == "drop":
        return "ALTER TABLE %s DROP COLUMN %s;" % (t, quote(kind, column["name"]))
    if action == "rename":
        if kind == "sqlserver":
            return "EXEC sp_rename '%s.%s', '%s', 'COLUMN';" % (table, column["name"], column["new_name"])
        return "ALTER TABLE %s RENAME COLUMN %s TO %s;" % (t, quote(kind, column["name"]), quote(kind, column["new_name"]))
    raise DbError("Unknown change: %s" % action)


# ---------------------------------------------------------------- editor protocol


def serve():
    engine = Engine()
    methods = {
        "open": lambda p: engine.open(p["id"], p["config"]),
        "close": lambda p: engine.close(p["id"]),
        "schema": lambda p: engine.schema(p["id"]),
        "query": lambda p: engine.query(p["id"], p["sql"], p.get("allow_write", False), p.get("limit", MAX_ROWS)),
        "rows": lambda p: engine.rows(p["id"], p["table"], p.get("limit", 200), p.get("offset", 0), p.get("where", ""), p.get("order", "")),
        "update_cell": lambda p: engine.update_cell(p["id"], p["table"], p["keys"], p["column"], p["value"]),
        "insert_row": lambda p: engine.insert_row(p["id"], p["table"], p["values"]),
        "delete_rows": lambda p: engine.delete_rows(p["id"], p["table"], p["rows"]),
        "export_csv": lambda p: engine.export_csv(p["id"], p["sql"], p["path"]),
        "create_table_sql": lambda p: {"sql": create_table_sql(p["kind"], p["table"], p["columns"])},
        "alter_sql": lambda p: {"sql": alter_sql(p["kind"], p["table"], p["action"], p["column"])},
        "is_write": lambda p: {"write": is_write(p["sql"])},
    }
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        mid = msg.get("id")
        try:
            method = methods.get(msg.get("method"))
            if method is None:
                raise DbError("Unknown method %s" % msg.get("method"))
            out = {"id": mid, "result": method(msg.get("params") or {})}
        except DbError as e:
            out = {"id": mid, "error": str(e)}
        except Exception as e:
            out = {"id": mid, "error": "%s: %s" % (type(e).__name__, e)}
        sys.stdout.write(json.dumps(out) + "\n")
        sys.stdout.flush()


# ---------------------------------------------------------------- MCP for agents


def load_connections(path):
    """Connections file written by ForgeCode: [{name, kind, …, password_command?}]."""
    with open(path, encoding="utf-8") as f:
        conns = json.load(f)
    for c in conns:
        # Passwords stay in the keychain; ForgeCode writes the command that reads one.
        if c.get("password_command") and not c.get("password"):
            try:
                import subprocess

                c["password"] = subprocess.run(c["password_command"], capture_output=True, text=True, timeout=10).stdout.strip()
            except Exception:
                c["password"] = ""
    return {c["name"]: c for c in conns}


def confirm_write(name, sql):
    """Ask the user in ForgeCode before an agent writes (FORGE_CONFIRM_URL, a local endpoint)."""
    url = os.environ.get("FORGE_CONFIRM_URL")
    if not url:
        return False
    body = json.dumps({"connection": name, "sql": sql}).encode()
    req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json", "Authorization": "Bearer " + os.environ.get("FORGE_CONFIRM_KEY", "")})
    try:
        with urllib.request.urlopen(req, timeout=600) as r:
            return json.loads(r.read().decode()).get("allowed") is True
    except Exception:
        return False


TOOLS = [
    {"name": "db_list_connections", "description": "List the project's database connections (name, kind, database).", "inputSchema": {"type": "object", "properties": {}}},
    {"name": "db_schema", "description": "Tables and views of a connection with their columns, types and primary keys.",
     "inputSchema": {"type": "object", "properties": {"connection": {"type": "string"}}, "required": ["connection"]}},
    {"name": "db_query", "description": "Run a read-only SQL query (SELECT, WITH, SHOW, EXPLAIN) and return up to 200 rows.",
     "inputSchema": {"type": "object", "properties": {"connection": {"type": "string"}, "sql": {"type": "string"}}, "required": ["connection", "sql"]}},
    {"name": "db_execute", "description": "Run a statement that changes data or structure (INSERT, UPDATE, DELETE, CREATE, ALTER, DROP…). The user must approve it in ForgeCode first.",
     "inputSchema": {"type": "object", "properties": {"connection": {"type": "string"}, "sql": {"type": "string"}}, "required": ["connection", "sql"]}},
]


def mcp(path):
    engine = Engine()

    def call(name, args):
        conns = load_connections(path)
        if name == "db_list_connections":
            return [{"name": c["name"], "kind": c["kind"], "database": c.get("database") or c.get("path"), "readonly": bool(c.get("readonly"))} for c in conns.values()]
        cname = args.get("connection", "")
        if cname not in conns:
            raise DbError("Unknown connection %r; known: %s" % (cname, ", ".join(conns) or "none"))
        cfg = conns[cname]
        if cname not in engine.connections:
            engine.open(cname, cfg)
        if name == "db_schema":
            return engine.schema(cname)
        if name == "db_query":
            if is_write(args["sql"]):
                raise DbError("db_query only reads; use db_execute to change data (the user approves it)")
            return engine.query(cname, args["sql"], limit=200)
        if name == "db_execute":
            if cfg.get("readonly"):
                raise DbError("This connection is read-only")
            if not confirm_write(cname, args["sql"]):
                raise DbError("The user did not approve this statement")
            return engine.query(cname, args["sql"], allow_write=True, limit=200)
        raise DbError("Unknown tool %s" % name)

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        method, mid = msg.get("method"), msg.get("id")
        if mid is None:
            continue  # notifications (initialized…)
        if method == "initialize":
            result = {"protocolVersion": msg.get("params", {}).get("protocolVersion", "2024-11-05"), "capabilities": {"tools": {}},
                      "serverInfo": {"name": "forgecode-databases", "version": "1.0.0"}}
        elif method == "tools/list":
            result = {"tools": TOOLS}
        elif method == "tools/call":
            p = msg.get("params") or {}
            try:
                data = call(p.get("name"), p.get("arguments") or {})
                result = {"content": [{"type": "text", "text": json.dumps(data, ensure_ascii=False, default=str)}]}
            except DbError as e:
                result = {"content": [{"type": "text", "text": str(e)}], "isError": True}
            except Exception as e:
                result = {"content": [{"type": "text", "text": "%s: %s" % (type(e).__name__, e)}], "isError": True}
        elif method == "ping":
            result = {}
        else:
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown method"}}) + "\n")
            sys.stdout.flush()
            continue
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    if len(sys.argv) >= 2 and sys.argv[1] == "serve":
        serve()
    elif len(sys.argv) >= 3 and sys.argv[1] == "mcp":
        mcp(sys.argv[2])
    else:
        print(__doc__)
        sys.exit(2)
