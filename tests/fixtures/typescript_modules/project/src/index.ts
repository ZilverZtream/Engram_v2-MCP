import { save as persist } from "./primary";
import * as backup from "./alternate";
export function run_primary(): number { return persist(); }
export function run_alternate(): number { return backup.save(); }
export function run_local(): number { return helper(); }
function helper(): number { return 3; }
