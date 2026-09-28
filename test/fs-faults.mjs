// Preloaded into sync.mjs (node --import) to stage lock races and I/O faults
// deterministically. FS_FAULT picks one:
//   steal-race  another stealer evicts the stale lock and takes a fresh one
//               just before our takeover rename
//   pid-write   the first pid write into a new lock fails
import fs from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import { basename, dirname, join } from "node:path";

const fault = process.env.FS_FAULT;

if (fault === "steal-race") {
  const rename = fs.renameSync;
  let raced = false;
  fs.renameSync = (from, to) => {
    if (!raced && basename(String(from)) === ".lock" && basename(String(to)).startsWith(".lock.stale-")) {
      raced = true;
      const other = join(dirname(String(from)), ".lock.stale-other");
      rename(from, other);
      fs.unlinkSync(other);
      fs.writeFileSync(from, "88888", { flag: "wx" });
    }
    return rename(from, to);
  };
}

if (fault === "pid-write") {
  const open = fs.openSync;
  const write = fs.writeSync;
  let lockFd;
  fs.openSync = (path, ...rest) => {
    const fd = open(path, ...rest);
    if (lockFd === undefined && basename(String(path)) === ".lock") lockFd = fd;
    return fd;
  };
  fs.writeSync = (fd, ...rest) => {
    if (fd === lockFd) {
      lockFd = null; // fail once
      throw Object.assign(new Error("EIO: injected pid write failure"), { code: "EIO" });
    }
    return write(fd, ...rest);
  };
}

syncBuiltinESMExports();
