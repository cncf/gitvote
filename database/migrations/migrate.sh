#!/bin/sh

schemaVersionTable=version_schema

echo "- Applying schema migrations.."
cd schema
tern status --config $TERN_CONF --version-table $schemaVersionTable
tern migrate --config $TERN_CONF --version-table $schemaVersionTable
if [ $? -ne 0 ]; then exit 1; fi
echo "Done"
