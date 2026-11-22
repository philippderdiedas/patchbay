-- SPDX-License-Identifier: GPL-3.0-or-later
-- Call targets of kind "app" (func_odbc TARGET joins apps for the app number): the asterisk role
-- needs these columns. Without them every TARGET lookup failed (IVR options, DID targets, forwards),
-- not only app targets.
GRANT SELECT (id, number, deleted_at) ON apps TO asterisk;
