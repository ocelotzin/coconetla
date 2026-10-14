use std::env;
use std::io::Write;
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const UNDEF: usize = usize::MAX;

#[derive(Clone, Copy)]
struct Tr {
    write: u8,
    dir: i8,
    next: usize, // 0..n-1 estados, n = parada, UNDEF = sin definir
}
const EMPTY: Tr = Tr { write: 0, dir: 1, next: UNDEF };

enum Out {
    Halt(bool),     // paró; true si cumple k unos contiguos
    Undef(usize),   // llegó a la transición sin definir con este índice
    Timeout,
}

struct Ctx {
    n: usize,
    k: usize,
    max_steps: usize,
    max_results: u64,
    split: usize,
    stop: AtomicBool,
    found: AtomicU64,
    nodes: AtomicU64,
    timeouts: AtomicU64,
    out: Mutex<()>,
}

type Task = (Vec<Tr>, usize, usize); // tabla, estados descubiertos, transiciones definidas

fn print_help(prog: &str) {
    println!(
"Buscador de máquinas de Turing (cinta blanca) que producen k unos contiguos.
Genera código listo para pegar en https://turingmachinesimulator.com/

USO:
    {prog} <n> <k> [max_pasos] [max_resultados] [semilla] [hilos]
    {prog} --help | -h

ARGUMENTOS:
    <n>               Número de estados (q0..q{{n-1}}), más el estado de parada qf.
                      Obligatorio. Entero >= 1.
    <k>               Número de unos que debe haber en la cinta al parar, y
                      deben ser contiguos. Obligatorio.
    [max_pasos]       Límite de pasos por máquina; las que no paran antes se
                      descartan. Por defecto: 1000.
    [max_resultados]  Se detiene tras encontrar este número de máquinas.
                      0 = todas. Por defecto: 0.
    [semilla]         Semilla que decide por dónde empieza la exploración.
                      Por defecto: aleatoria (distinta en cada ejecución).
    [hilos]           Número de hilos de ejecución.
                      Por defecto: todos los núcleos disponibles.

SALIDA:
    stdout  Código de las máquinas encontradas (formato del simulador).
    stderr  Estadísticas: tareas, nodos explorados, timeouts y encontradas.

NOTAS:
    - Alfabeto de 2 símbolos: blanco (_) y 1. Movimientos: < y >.
    - Se aplican poda por espejo y numeración canónica de estados.
    - Los estados no usados se omiten y las transiciones no usadas se
      rellenan con 'qf,_,>'.
    - Con varios hilos el orden de salida puede variar; para reproducir una
      ejecución usa la misma semilla y 1 hilo.

EJEMPLOS:
    {prog} 3 6                      # n=3, k=6, valores por defecto
    {prog} 5 10 500 20              # n=5, k=10, 500 pasos, 20 máquinas
    {prog} 4 13 2000 0 42 8         # todas, semilla 42, 8 hilos
    {prog} 3 6 > salida.txt         # guardar el código en un archivo",
        prog = prog
    );
}

fn run(table: &[Tr], cx: &Ctx, tape: &mut [u8]) -> Out {
    let n = cx.n;
    let mut head = cx.max_steps + 1;
    let (mut lo, mut hi) = (head, head);
    let mut state = 0usize;
    let mut result = Out::Timeout;

    for _ in 0..cx.max_steps {
        let idx = state * 2 + tape[head] as usize;
        let tr = table[idx];
        if tr.next == UNDEF {
            result = Out::Undef(idx);
            break;
        }
        tape[head] = tr.write;
        if tr.dir > 0 { head += 1 } else { head -= 1 }
        lo = lo.min(head);
        hi = hi.max(head);
        state = tr.next;
        if state == n {
            let (mut ones, mut first, mut last) = (0usize, usize::MAX, 0usize);
            for i in lo..=hi {
                if tape[i] == 1 {
                    ones += 1;
                    if first == usize::MAX { first = i }
                    last = i;
                }
            }
            let ok = ones == cx.k && (cx.k == 0 || last - first + 1 == cx.k);
            result = Out::Halt(ok);
            break;
        }
    }
    for i in lo..=hi {
        tape[i] = 0;
    }
    result
}

fn format_machine(table: &[Tr], disc: usize, cx: &Ctx, id: u64) -> String {
    let n = cx.n;
    let name_of = |s: usize| if s == n { "qf".to_string() } else { format!("q{}", s) };
    let sym = |w: u8| if w == 1 { "1" } else { "_" };
    let mut s = format!("name: TM_n{}_k{}_#{}\ninit: q0\naccept: qf\n\n", n, cx.k, id);
    for st in 0..disc {
        for r in 0..2usize {
            let tr = table[st * 2 + r];
            let (nx, w, d) = if tr.next == UNDEF {
                (n, 0u8, 1i8) // transición no usada: relleno
            } else {
                (tr.next, tr.write, tr.dir)
            };
            s += &format!(
                "q{},{}\n{},{},{}\n\n",
                st, sym(r as u8), name_of(nx), sym(w), if d > 0 { ">" } else { "<" }
            );
        }
    }
    s += "// ----------------------------------------\n";
    s
}

fn report(table: &[Tr], disc: usize, cx: &Ctx) {
    let id = cx.found.fetch_add(1, Ordering::SeqCst) + 1;
    if cx.max_results > 0 && id > cx.max_results {
        cx.stop.store(true, Ordering::Relaxed);
        return;
    }
    let text = format_machine(table, disc, cx, id);
    {
        let _g = cx.out.lock().unwrap();
        std::io::stdout().lock().write_all(text.as_bytes()).unwrap();
    }
    if cx.max_results > 0 && id >= cx.max_results {
        cx.stop.store(true, Ordering::Relaxed);
    }
}

fn explore(
    table: &mut Vec<Tr>,
    disc: usize,
    defined: usize,
    cx: &Ctx,
    tape: &mut [u8],
    tasks: &mut Option<Vec<Task>>,
) {
    if cx.stop.load(Ordering::Relaxed) {
        return;
    }
    cx.nodes.fetch_add(1, Ordering::Relaxed);
    match run(table, cx, tape) {
        Out::Halt(ok) => {
            if ok {
                report(table, disc, cx);
            }
        }
        Out::Timeout => {
            cx.timeouts.fetch_add(1, Ordering::Relaxed);
        }
        Out::Undef(idx) => {
            if let Some(t) = tasks.as_mut() {
                if defined == cx.split {
                    t.push((table.clone(), disc, defined));
                    return;
                }
            }
            let n = cx.n;
            for next in 0..=n {
                if next < n && next > disc {
                    continue; // no saltar números de estado
                }
                let new_disc = if next == disc && next < n { disc + 1 } else { disc };
                for write in 0..2u8 {
                    for &dir in &[1i8, -1i8] {
                        if idx == 0 && dir < 0 {
                            continue; // espejo
                        }
                        if next == n && dir < 0 {
                            continue; // la dirección de parada es irrelevante
                        }
                        table[idx] = Tr { write, dir, next };
                        explore(table, new_disc, defined + 1, cx, tape, tasks);
                    }
                }
            }
            table[idx] = EMPTY;
        }
    }
}

fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let prog = args.first().map_or("tm_search", |s| s.as_str());

    if args.iter().skip(1).any(|a| a == "--help" || a == "-h") {
        print_help(prog);
        return;
    }
    if args.len() < 3 {
        eprintln!("Error: faltan argumentos <n> y <k>.\n");
        eprintln!("Uso: {} <n> <k> [max_pasos] [max_resultados] [semilla] [hilos]", prog);
        eprintln!("Prueba '{} --help' para más información.", prog);
        process::exit(1);
    }

    let parse = |i: usize, name: &str| -> Option<u64> {
        args.get(i).map(|s| {
            s.parse::<u64>().unwrap_or_else(|_| {
                eprintln!("Error: '{}' no es un valor válido para <{}>.", s, name);
                eprintln!("Prueba '{} --help' para más información.", prog);
                process::exit(1);
            })
        })
    };

    let n = parse(1, "n").unwrap() as usize;
    let k = parse(2, "k").unwrap() as usize;
    let max_steps = parse(3, "max_pasos").map_or(1000, |v| v as usize);
    let max_results = parse(4, "max_resultados").unwrap_or(0);
    let seed = parse(5, "semilla").unwrap_or_else(|| {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
    });
    let threads = parse(6, "hilos")
        .map_or_else(|| thread::available_parallelism().map_or(1, |p| p.get()), |v| v as usize);

    if n < 1 || threads < 1 {
        eprintln!("Error: <n> y [hilos] deben ser >= 1.");
        process::exit(1);
    }

    let cx = Ctx {
        n, k, max_steps, max_results,
        split: 6.min(2 * n),
        stop: AtomicBool::new(false),
        found: AtomicU64::new(0),
        nodes: AtomicU64::new(0),
        timeouts: AtomicU64::new(0),
        out: Mutex::new(()),
    };

    // 1) Fase de reparto: explorar hasta `split` transiciones definidas.
    let mut tasks_opt = Some(Vec::new());
    {
        let mut table = vec![EMPTY; 2 * n];
        let mut tape = vec![0u8; 2 * max_steps + 3];
        explore(&mut table, 1, 0, &cx, &mut tape, &mut tasks_opt);
    }
    let mut tasks = tasks_opt.unwrap();
    if !tasks.is_empty() {
        let rot = (splitmix64(seed) % tasks.len() as u64) as usize;
        tasks.rotate_left(rot);
    }
    eprintln!("Tareas: {} | hilos: {} | semilla: {}", tasks.len(), threads, seed);

    // 2) Hilos de trabajo.
    let next_task = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut tape = vec![0u8; 2 * max_steps + 3];
                let mut none: Option<Vec<Task>> = None;
                loop {
                    if cx.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next_task.fetch_add(1, Ordering::Relaxed);
                    if i >= tasks.len() {
                        break;
                    }
                    let (t, disc, defined) = &tasks[i];
                    let mut table = t.clone();
                    explore(&mut table, *disc, *defined, &cx, &mut tape, &mut none);
                }
            });
        }
    });

    let f = cx.found.load(Ordering::SeqCst);
    let f = if max_results > 0 { f.min(max_results) } else { f };
    eprintln!(
        "Nodos: {} | timeouts: {} | encontradas: {}",
        cx.nodes.load(Ordering::Relaxed),
        cx.timeouts.load(Ordering::Relaxed),
        f
    );
}
