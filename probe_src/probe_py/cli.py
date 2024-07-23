import io
import os
import sys
import tempfile
import subprocess
import typing_extensions
import tarfile
import pathlib
import typer
import shutil
import rich
from . import parse_probe_log
from . import analysis
from . import util

rich.traceback.install(show_locals=False)


project_root = pathlib.Path(__file__).resolve().parent.parent

A = typing_extensions.Annotated

app = typer.Typer()

def transcribe(probe_dir: pathlib.Path, output: pathlib.Path, debug: bool = False) -> None:
    """
    Transcribe the recorded data from PROBE_DIR into OUTPUT.
    """
    probe_log_tar_obj = tarfile.open(name=str(output), mode="x:gz")
    probe_log_tar_obj.add(probe_dir, arcname="")
    probe_log_tar_obj.addfile(
        util.default_tarinfo("README"),
        fileobj=io.BytesIO(b"This archive was generated by PROBE."),
    )
    probe_log_tar_obj.close()
    if debug:
        print()
        print("PROBE log files:")
        for path in probe_dir.glob("**/*"):
            if not path.is_dir():
                print(path, path.stat().st_size)
        print()
    shutil.rmtree(probe_dir)

@app.command()    
def transcribe_only(
        input_dir: pathlib.Path,
        output: pathlib.Path = pathlib.Path("probe_log"),
        debug: bool = typer.Option(default=False, help="Run in verbose mode"),
) -> None:
    """
    Transcribe the recorded data from INPUT_DIR into OUTPUT.
    """
    transcribe(input_dir, output, debug)

@app.command(
    context_settings=dict(
        ignore_unknown_options=True,
    ),
)
def record(
        cmd: list[str],
        gdb: bool = typer.Option(default=False, help="Run in GDB"),
        debug: bool = typer.Option(default=False, help="Run verbose & debug build of libprobe"),
        make: bool = typer.Option(default=False, help="Run make prior to executing"),
        output: pathlib.Path = pathlib.Path("probe_log"),
        no_transcribe: bool = typer.Option(default=False, help="Only execute without transcribing"),
) -> None:
    """
    Execute CMD... and optionally record its provenance into OUTPUT.
    """
    if make:
        proc = subprocess.run(
            ["make", "--directory", str(project_root / "libprobe"), "all"],
        )
        if proc.returncode != 0:
            typer.secho("Make failed", fg=typer.colors.RED)
            raise typer.Abort()
    if output.exists():
        output.unlink()
    libprobe = project_root / "libprobe/build" / ("libprobe-dbg.so" if debug or gdb else "libprobe.so")
    if not libprobe.exists():
        typer.secho(f"Libprobe not found at {libprobe}", fg=typer.colors.RED)
        raise typer.Abort()
    ld_preload = str(libprobe) + (":" + os.environ["LD_PRELOAD"] if "LD_PRELOAD" in os.environ else "")
    probe_dir = pathlib.Path(tempfile.mkdtemp(prefix=f"probe_log_{os.getpid()}"))
    if gdb:
        subprocess.run(
            ["gdb", "--args", "env", f"__PROBE_DIR={probe_dir}", f"LD_PRELOAD={ld_preload}", *cmd],
        )
    else:
        if debug:
            typer.secho(f"Running {cmd} with libprobe into {probe_dir}", fg=typer.colors.GREEN)
        proc = subprocess.run(
            cmd,
            env={**os.environ, "LD_PRELOAD": ld_preload, "__PROBE_DIR": str(probe_dir)},
        )

        if no_transcribe:
            typer.secho(f"Temporary probe directory: {probe_dir}", fg=typer.colors.YELLOW)
            raise typer.Exit(proc.returncode)
        
        transcribe(probe_dir, output, debug)
        raise typer.Exit(proc.returncode)

@app.command()
def process_graph(
        input: pathlib.Path = pathlib.Path("probe_log"),
) -> None:
    """
    Write a process graph from PROBE_LOG in DOT/graphviz format.
    """
    if not input.exists():
        typer.secho(f"INPUT {input} does not exist\nUse `PROBE record --output {input} CMD...` to rectify", fg=typer.colors.RED)
        raise typer.Abort()
    probe_log_tar_obj = tarfile.open(input, "r")
    prov_log = parse_probe_log.parse_probe_log_tar(probe_log_tar_obj)
    probe_log_tar_obj.close()
    console = rich.console.Console(file=sys.stderr)
    process_graph = analysis.provlog_to_digraph(prov_log)
    for warning in analysis.validate_provlog(prov_log):
        console.print(warning, style="red")
    rich.traceback.install(show_locals=False) # Figure out why we need this
    process_graph = analysis.provlog_to_digraph(prov_log)
    for warning in analysis.validate_hb_graph(prov_log, process_graph):
        console.print(warning, style="red")
    print(analysis.digraph_to_pydot_string(prov_log, process_graph))
    

@app.command()
def dump(
        input: pathlib.Path = pathlib.Path("probe_log"),
) -> None:
    """
    Write the data from PROBE_LOG in a human-readable manner.
    """
    if not input.exists():
        typer.secho(f"INPUT {input} does not exist\nUse `PROBE record --output {input} CMD...` to rectify", fg=typer.colors.RED)
        raise typer.Abort()
    probe_log_tar_obj = tarfile.open(input, "r")
    processes_prov_log = parse_probe_log.parse_probe_log_tar(probe_log_tar_obj)
    probe_log_tar_obj.close()
    for pid, process in processes_prov_log.processes.items():
        print(pid)
        for exid, exec_epoch in process.exec_epochs.items():
            print(pid, exid)
            for tid, thread in exec_epoch.threads.items():
                print(pid, exid, tid)
                for op_no, op in enumerate(thread.ops):
                    print(pid, exid, tid, op_no, op.data)
                print()

if __name__ == "__main__":
    app()
