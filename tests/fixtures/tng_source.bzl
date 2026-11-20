"""One TNG source synced on its own (`sync_one_source.py`): its ingest,
its render and a grid index over it alone, as `<name>/index.sqlite`.

A source here is checked without the `:ingested_tng` fan-in: a change to
it, or to its fixture, rebuilds and re-checks it and nothing else.
"""

def tng_source(name, source_id, type, ingest_params, inputs, **kwargs):
    """`inputs` maps a `@name@` in `ingest_params` to a label whose one
    file is put there."""
    native.genrule(
        name = name,
        srcs = inputs.values() + [
            "//datalib/backend/dag:datalib_dag_bin",
            "//datalib/backend/datalib_step:datalib_step",
        ],
        outs = [name + "/index.sqlite"],
        tools = [
            "//tests/fixtures:quiet.sh",
            "//tests/fixtures:sync_drivers.py",
            "//tests/fixtures:sync_one_source.py",
        ],
        cmd = " ".join([
            "$(execpath //tests/fixtures:quiet.sh) $(RULEDIR)/" + name + ".log",
            "python3 $(execpath //tests/fixtures:sync_one_source.py)",
            "--dag $(execpath //datalib/backend/dag:datalib_dag_bin)",
            "--step $(execpath //datalib/backend/datalib_step:datalib_step)",
            "--now '2369-04-15T00:00:00+00:00'",
            "--workspace $(RULEDIR)/" + name + "_workspace",
            "--id " + source_id,
            "--type " + type,
            "--ingest-params '" + ingest_params + "'",
        ] + [
            "--input %s=$(execpath %s)" % (key, label)
            for key, label in inputs.items()
        ] + [
            "--index-out $(execpath " + name + "/index.sqlite)",
        ]),
        **kwargs
    )
