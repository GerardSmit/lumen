// node:sqlite core synchronous API over the actual shared SQLite native operations.
// Unsupported authorizers, aggregate functions, sessions and backups are deliberately absent.
{
  const S = globalThis.__sqlite;
  const INTERNAL = Symbol("node:sqlite statement");
  let selectedLibrary;
  function failure(error) {
    if (error && error.__sqlite) {
      const result = new Error(error.message);
      result.code = "ERR_SQLITE_ERROR";
      result.errcode = error.errno;
      result.errstr = error.code;
      throw result;
    }
    throw error;
  }
  function call(operation, ...args) {
    try { return operation(...args); } catch (error) { failure(error); }
  }
  function integer(value, bigints) {
    if (typeof value !== "bigint" || bigints) return value;
    const number = Number(value);
    if (!Number.isSafeInteger(number)) throw new RangeError("SQLite integer exceeds JavaScript's safe integer range");
    return number;
  }
  class StatementSync {
    constructor(token, database, sql) {
      if (token !== INTERNAL) throw new TypeError("StatementSync cannot be constructed directly");
      this._db = database;
      this._id = call(S.prepare, database._id, sql);
      this._bigints = database._options.readBigInts === true;
      this._bare = database._options.allowBareNamedParameters !== false;
      this._unknown = database._options.allowUnknownNamedParameters === true;
      this._arrays = database._options.returnArrays === true;
      this.sourceSQL = sql;
    }
    _bind(args) {
      this._db._assertOpen();
      call(S.reset, this._id, true);
      const count = S.bindParameterCount(this._id);
      let named = null, position = 0;
      if (args[0] !== null && typeof args[0] === "object" && !ArrayBuffer.isView(args[0])) named = args.shift();
      if (named) {
        for (const key of Object.keys(named)) {
          let index = S.bindParameterIndex(this._id, key);
          if (!index && this._bare) {
            for (let i = 1; i <= count; i++) {
              const parameter = S.bindParameterName(this._id, i);
              if (parameter && parameter.slice(1) === key) {
                if (index) throw new Error(`Ambiguous named parameter '${key}'`);
                index = i;
              }
            }
          }
          if (!index) {
            if (this._unknown) continue;
            throw new Error(`Unknown named parameter '${key}'`);
          }
          call(S.bind, this._id, index, named[key]);
        }
      }
      for (let i = 1; i <= count; i++) {
        if (named && S.bindParameterName(this._id, i)) continue;
        if (position < args.length) call(S.bind, this._id, i, args[position++]);
      }
      if (position !== args.length) throw new Error("Too many parameter values were provided");
    }
    _row() {
      const values = S.row(this._id, true).map(value => integer(value, this._bigints));
      if (this._arrays) return values;
      const names = S.columnNames(this._id), row = Object.create(null);
      for (let i = 0; i < names.length; i++) row[names[i]] = values[i];
      return row;
    }
    get(...args) {
      this._bind(args);
      try { return call(S.step, this._id) ? this._row() : undefined; }
      finally { call(S.reset, this._id, false); }
    }
    all(...args) {
      this._bind(args);
      const rows = [];
      try { while (call(S.step, this._id)) rows.push(this._row()); return rows; }
      finally { call(S.reset, this._id, false); }
    }
    run(...args) {
      this._bind(args);
      try {
        while (call(S.step, this._id)) {}
        return {
          changes: this._bigints ? BigInt(S.changes(this._db._id)) : S.changes(this._db._id),
          lastInsertRowid: integer(S.lastInsertRowid(this._db._id, true), this._bigints),
        };
      } finally { call(S.reset, this._id, false); }
    }
    *iterate(...args) {
      this._bind(args);
      try { while (call(S.step, this._id)) yield this._row(); }
      finally { call(S.reset, this._id, false); }
    }
    columns() { this._db._assertOpen(); return call(S.columns, this._id).map(column => Object.assign(Object.create(null), column)); }
    setReadBigInts(value) { this._bigints = Boolean(value); }
    setAllowBareNamedParameters(value) { this._bare = Boolean(value); }
    setAllowUnknownNamedParameters(value) { this._unknown = Boolean(value); }
    setReturnArrays(value) { this._arrays = Boolean(value); }
    get expandedSQL() { this._db._assertOpen(); return call(S.expandedSql, this._id); }
  }
  class DatabaseSync {
    constructor(location, options = {}) {
      if (typeof location !== "string") throw new TypeError("SQLite location must be a string");
      for (const option of ["open", "readOnly", "enableForeignKeyConstraints", "enableDoubleQuotedStringLiterals", "allowExtension", "readBigInts", "returnArrays", "allowBareNamedParameters", "allowUnknownNamedParameters", "defensive"]) {
        if (options[option] !== undefined && typeof options[option] !== "boolean") throw new TypeError(`${option} must be a boolean`);
      }
      if (options.limits !== undefined) throw new Error("node:sqlite custom limits are not implemented");
      if (options.timeout !== undefined && (!Number.isInteger(options.timeout) || options.timeout < 0)) throw new RangeError("SQLite timeout must be a non-negative integer");
      this._allowExtension = options.allowExtension === true;
      this._extensionEnabled = this._allowExtension;
      this._location = location;
      this._options = options;
      this._id = null;
      if (options.open !== false) this.open();
    }
    _assertOpen() { if (this._id === null) throw new Error("database is not open"); }
    open() {
      if (this._id !== null) throw new Error("database is already open");
      const selected = process.env.LUMEN_SQLITE_LIBRARY;
      if (selected && selected !== selectedLibrary) {
        call(S.setCustomSQLite, selected);
        selectedLibrary = selected;
      }
      this._id = call(S.open, this._location, (this._options.readOnly ? 1 : 6) | 64);
      try {
        call(S.enableLoadExtension, this._id, this._allowExtension);
        call(S.doubleQuotedStringLiterals, this._id, this._options.enableDoubleQuotedStringLiterals === true);
        call(S.defensive, this._id, this._options.defensive !== false);
        this.exec(`PRAGMA foreign_keys=${this._options.enableForeignKeyConstraints === false ? 0 : 1}`);
        if (this._options.timeout !== undefined) {
          const timeout = this._options.timeout;
          if (!Number.isInteger(timeout) || timeout < 0) throw new RangeError("SQLite timeout must be a non-negative integer");
          this.exec(`PRAGMA busy_timeout=${timeout}`);
        }
      } catch (error) { this.close(); throw error; }
    }
    function(name, options, callback) {
      this._assertOpen();
      if (typeof options === "function") {callback=options;options={};}
      if (typeof name !== "string" || typeof callback !== "function" || options===null || typeof options!=="object") throw new TypeError("SQLite function requires a name, options and callback");
      for (const key of ["deterministic","directOnly","useBigIntArguments","varargs"]) {
        if(options[key]!==undefined && typeof options[key]!=="boolean") throw new TypeError(`${key} must be a boolean`);
      }
      const count=options.varargs ? -1 : callback.length;
      if(!Number.isInteger(count) || count< -1 || count>1000) throw new RangeError("SQLite function argument count is invalid");
      call(S.function,this._id,name,callback,count,(options.deterministic?0x800:0)|(options.directOnly?0x80000:0),options.useBigIntArguments===true);
    }
    enableLoadExtension(allow) {
      this._assertOpen();
      if(typeof allow !== "boolean") {const error=new TypeError('The "allow" argument must be a boolean.');error.code="ERR_INVALID_ARG_TYPE";throw error;}
      if(allow && !this._allowExtension) {const error=new Error("Cannot enable extension loading because it was disabled at database creation.");error.code="ERR_INVALID_STATE";throw error;}
      call(S.enableLoadExtension,this._id,allow);
      this._extensionEnabled = allow;
    }
    loadExtension(path) {
      this._assertOpen();
      if (!this._extensionEnabled) {const error=new Error("Extension loading is disabled for this database.");error.code="ERR_INVALID_STATE";throw error;}
      if (typeof path !== "string") {const error=new TypeError('The "path" argument must be a string.');error.code="ERR_INVALID_ARG_TYPE";throw error;}
      call(S.loadExtension,this._id,path);
    }
    enableDefensive(enabled) { this._assertOpen(); if (typeof enabled !== "boolean") throw new TypeError("enabled must be a boolean"); call(S.defensive, this._id, enabled); }
    location(schema = "main") { this._assertOpen(); return call(S.location, this._id, String(schema)); }
    get isOpen() { return this._id !== null; }
    get isTransaction() { this._assertOpen(); return call(S.isTransaction, this._id); }
    exec(sql) { this._assertOpen(); call(S.exec, this._id, String(sql)); }
    prepare(sql) { this._assertOpen(); return new StatementSync(INTERNAL, this, String(sql)); }
    close() { this._assertOpen(); call(S.close, this._id); this._id = null; }
    [Symbol.dispose]() { if (this.isOpen) this.close(); }
  }
  const constants = { SQLITE_OK: 0, SQLITE_DENY: 1, SQLITE_IGNORE: 2 };
  __builtins.set("sqlite", { DatabaseSync, StatementSync, constants });
}
