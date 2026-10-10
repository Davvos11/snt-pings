/*
 * DPDK ICMPv6 pixel flooder for the Jinglepings canvas.
 *
 * Dumb, fast packet pump: reads one or more files of pre-built, fully-formed
 * Ethernet + IPv6 + ICMPv6 frames (62 bytes each) produced by the Rust side
 * (`afxdp --dump frames.bin`) and blasts them out of an Intel VF via the iavf
 * PMD, one TX queue per lcore.
 *
 * All protocol logic (pixel encoding, headers, ICMPv6 checksum, source MAC =
 * the VF's own MAC for anti-spoofing, and a *routable* source IP) lives in the
 * Rust generator. This program never parses or builds a packet.
 *
 * Build:   (see Makefile)   needs DPDK installed + `pkg-config libdpdk`
 * Run:     sudo ./pingflood -l 0-3 -n 4 -a 0000:06:00.0 -- <frames...>
 *            -l   lcores to use (one TX queue each)
 *            -a   PCI address of the VF (after binding it to vfio-pci)
 *            --   everything after is this app's args: one or more frame files
 *
 * Live reload (for a webcam / changing image):
 *   Prefix a frame file with '@' to WATCH it. A background thread re-reads the
 *   file whenever its mtime changes and hot-swaps it in with no restart, so a
 *   separate poller (see webcam.sh) can keep it fresh:
 *     sudo ./pingflood -l 0-3 -n 4 -a 0000:06:00.0 -- @frames/webcam.bin
 *   The poller must write the new frames and then atomically rename over the
 *   file (same filesystem) so a half-written file is never observed.
 *
 * Core assignment:
 *   1 file     : every core floods it.
 *   N files    : the first N-1 files get ONE core each; the last file gets all
 *                remaining cores. So `-- A.bin B.bin` => A on 1 core, B on the
 *                rest (e.g. 3 cores on a 4-core box).
 *
 * Each core cycles the whole current frame table (with a per-core phase offset).
 * For a fixed total packet rate the per-pixel refresh rate is the same whether
 * cores partition the table or overlap on it, so overlapping keeps reload
 * trivially correct even when a reloaded image has a different pixel count.
 */

#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <pthread.h>
#include <unistd.h>
#include <time.h>
#include <sys/stat.h>

#include <rte_eal.h>
#include <rte_ethdev.h>
#include <rte_mbuf.h>
#include <rte_memcpy.h>
#include <rte_cycles.h>
#include <rte_lcore.h>

#define FRAME_LEN 62
#define TX_BURST 64
#define NB_TXD 1024
#define NB_RXD 128
#define MBUF_CACHE 256
#define MAX_IMAGES 16
#define RELOAD_POLL_US 200000   /* how often the watcher stats watched files */
#define RELOAD_GRACE_US 200000  /* wait after a swap before freeing the old table */

static volatile int force_quit;

/* An immutable, atomically-swappable frame table. */
struct frame_table {
    uint8_t *frames; /* n * FRAME_LEN bytes */
    uint32_t n;
};

/* One image slot. `tbl` is swapped atomically by the watcher thread; the TX
 * loops read it (acquire) once per burst. */
struct image {
    struct frame_table *_Atomic tbl;
    const char *path;
    int watch;        /* reload when the file mtime changes */
    time_t mtime;     /* last-loaded mtime (watcher thread only) */
};
static struct image g_img[MAX_IMAGES];
static int g_nimg;

static struct rte_mempool *g_mp;
static uint16_t g_port;

/* Per-lcore work: which TX queue, which image, and a starting phase offset. */
struct lcore_cfg {
    uint16_t queue_id;
    int      img;
    uint32_t phase;
    int      active;
};
static struct lcore_cfg g_cfg[RTE_MAX_LCORE];

static void handle_signal(int sig)
{
    (void)sig;
    force_quit = 1;
}

/* Read a dump file (N * 62 bytes) into a freshly allocated frame table.
 * Returns NULL on any error (caller decides whether that is fatal). */
static struct frame_table *load_table(const char *path)
{
    FILE *f = fopen(path, "rb");
    if (!f)
        return NULL;

    fseek(f, 0, SEEK_END);
    long sz = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (sz <= 0 || (sz % FRAME_LEN) != 0) {
        fclose(f);
        return NULL;
    }

    struct frame_table *t = malloc(sizeof(*t));
    if (!t) {
        fclose(f);
        return NULL;
    }
    t->frames = malloc(sz);
    if (!t->frames) {
        free(t);
        fclose(f);
        return NULL;
    }
    if (fread(t->frames, 1, sz, f) != (size_t)sz) {
        free(t->frames);
        free(t);
        fclose(f);
        return NULL;
    }
    fclose(f);
    t->n = (uint32_t)(sz / FRAME_LEN);
    return t;
}

/* Initial, fatal-on-error load of an image slot. */
static void load_image(int slot, const char *path, int watch)
{
    struct frame_table *t = load_table(path);
    if (!t)
        rte_exit(EXIT_FAILURE,
                 "cannot load frames file '%s' (missing, empty, or size not a "
                 "multiple of %d)\n", path, FRAME_LEN);

    atomic_store_explicit(&g_img[slot].tbl, t, memory_order_release);
    g_img[slot].path = path;
    g_img[slot].watch = watch;

    struct stat st;
    g_img[slot].mtime = (stat(path, &st) == 0) ? st.st_mtime : 0;

    printf("image %d: %u frames from %s%s\n", slot, t->n, path,
           watch ? " (watched)" : "");
}

/* Background thread: poll watched files for mtime changes and hot-swap them. */
static void *reloader(void *arg)
{
    (void)arg;
    while (!force_quit) {
        for (int i = 0; i < g_nimg; i++) {
            if (!g_img[i].watch)
                continue;
            struct stat st;
            if (stat(g_img[i].path, &st) != 0)
                continue;
            if (st.st_mtime == g_img[i].mtime)
                continue;

            struct frame_table *nt = load_table(g_img[i].path);
            if (!nt) {
                /* Likely caught mid-write; try again next tick without
                 * updating mtime so we keep retrying. */
                continue;
            }
            struct frame_table *old =
                atomic_exchange_explicit(&g_img[i].tbl, nt, memory_order_acq_rel);
            g_img[i].mtime = st.st_mtime;
            printf("image %d reloaded: %u frames from %s\n", i, nt->n,
                   g_img[i].path);

            /* Let any TX loop that already read `old` finish its burst copy
             * (microseconds) before freeing it. */
            usleep(RELOAD_GRACE_US);
            if (old) {
                free(old->frames);
                free(old);
            }
        }
        usleep(RELOAD_POLL_US);
    }
    return NULL;
}

/* Configure the port with one TX queue per lcore. */
static void port_init(uint16_t port, uint16_t nb_txq)
{
    struct rte_eth_conf port_conf;
    memset(&port_conf, 0, sizeof(port_conf));

    int ret = rte_eth_dev_configure(port, 1 /* rxq, unused */, nb_txq, &port_conf);
    if (ret < 0)
        rte_exit(EXIT_FAILURE, "dev_configure failed: %s\n", rte_strerror(-ret));

    uint16_t nb_rxd = NB_RXD, nb_txd = NB_TXD;
    ret = rte_eth_dev_adjust_nb_rx_tx_desc(port, &nb_rxd, &nb_txd);
    if (ret < 0)
        rte_exit(EXIT_FAILURE, "adjust_nb_rx_tx_desc failed: %s\n", rte_strerror(-ret));

    int socket = rte_eth_dev_socket_id(port);
    if (socket < 0)
        socket = (int)rte_socket_id(); /* non-NUMA / unknown: use local socket */

    for (uint16_t q = 0; q < nb_txq; q++) {
        ret = rte_eth_tx_queue_setup(port, q, nb_txd, socket, NULL);
        if (ret < 0)
            rte_exit(EXIT_FAILURE, "tx_queue_setup(%u) failed: %s\n", q,
                     rte_strerror(-ret));
    }

    /* One RX queue only because some PMDs require it to start; never polled. */
    ret = rte_eth_rx_queue_setup(port, 0, nb_rxd, socket, NULL, g_mp);
    if (ret < 0)
        rte_exit(EXIT_FAILURE, "rx_queue_setup failed: %s\n", rte_strerror(-ret));

    ret = rte_eth_dev_start(port);
    if (ret < 0)
        rte_exit(EXIT_FAILURE, "dev_start failed: %s\n", rte_strerror(-ret));

    struct rte_ether_addr mac;
    rte_eth_macaddr_get(port, &mac);
    printf("port %u up, MAC %02x:%02x:%02x:%02x:%02x:%02x, %u TX queues\n",
           port, mac.addr_bytes[0], mac.addr_bytes[1], mac.addr_bytes[2],
           mac.addr_bytes[3], mac.addr_bytes[4], mac.addr_bytes[5], nb_txq);
}

/* Per-lcore TX loop: allocate a burst, copy template frames in, transmit,
 * repeat forever. The current frame table is re-read (acquire) each burst so a
 * hot-swap by the watcher thread is picked up immediately. Fresh mbufs each
 * burst means the NIC owns and frees them after TX — no in-flight reuse
 * hazard. */
static int tx_loop(__rte_unused void *arg)
{
    unsigned lid = rte_lcore_id();
    struct lcore_cfg *cfg = &g_cfg[lid];
    if (!cfg->active)
        return 0;

    const uint16_t q = cfg->queue_id;
    struct image *img = &g_img[cfg->img];
    uint32_t idx = cfg->phase;

    struct rte_mbuf *bufs[TX_BURST];
    uint64_t sent = 0;
    const uint64_t hz = rte_get_tsc_hz();
    uint64_t last = rte_get_tsc_cycles();

    printf("lcore %u -> TX queue %u, image %d (phase %u)\n",
           lid, q, cfg->img, cfg->phase);

    while (!force_quit) {
        struct frame_table *t =
            atomic_load_explicit(&img->tbl, memory_order_acquire);
        const uint8_t *table = t->frames;
        const uint32_t n = t->n;
        if (idx >= n)
            idx = 0;

        if (rte_pktmbuf_alloc_bulk(g_mp, bufs, TX_BURST) != 0)
            continue; /* pool momentarily drained; retry */

        for (int i = 0; i < TX_BURST; i++) {
            uint8_t *d = rte_pktmbuf_mtod(bufs[i], uint8_t *);
            rte_memcpy(d, table + (size_t)idx * FRAME_LEN, FRAME_LEN);
            bufs[i]->data_len = FRAME_LEN;
            bufs[i]->pkt_len = FRAME_LEN;
            if (++idx >= n)
                idx = 0;
        }

        uint16_t nb = 0;
        while (nb < TX_BURST) {
            uint16_t sn = rte_eth_tx_burst(g_port, q, &bufs[nb], TX_BURST - nb);
            nb += sn;
            if (sn == 0 && force_quit)
                break;
        }
        sent += nb;
        for (uint16_t i = nb; i < TX_BURST; i++)
            rte_pktmbuf_free(bufs[i]);

        uint64_t now = rte_get_tsc_cycles();
        if (now - last >= hz) {
            printf("lcore %u q%u img%d: %.2f Mpps\n", lid, q, cfg->img,
                   (double)sent / ((double)(now - last) / hz) / 1e6);
            sent = 0;
            last = now;
        }
    }
    return 0;
}

int main(int argc, char **argv)
{
    int ret = rte_eal_init(argc, argv);
    if (ret < 0)
        rte_exit(EXIT_FAILURE, "EAL init failed\n");
    argc -= ret;
    argv += ret;

    if (argc < 2)
        rte_exit(EXIT_FAILURE,
                 "usage: %s [EAL args] -- <frames1> [frames2 ...]\n"
                 "  1 file : all cores flood it\n"
                 "  N files: first N-1 files get 1 core each, last gets the rest\n"
                 "  prefix a file with '@' to watch it and hot-reload on change\n",
                 argv[0]);

    g_nimg = argc - 1;
    if (g_nimg > MAX_IMAGES)
        rte_exit(EXIT_FAILURE, "too many images (max %d)\n", MAX_IMAGES);
    for (int i = 0; i < g_nimg; i++) {
        const char *arg = argv[1 + i];
        int watch = 0;
        if (arg[0] == '@') {       /* '@path' => watch this file */
            watch = 1;
            arg++;
        }
        load_image(i, arg, watch);
    }

    force_quit = 0;
    signal(SIGINT, handle_signal);
    signal(SIGTERM, handle_signal);

    if (rte_eth_dev_count_avail() == 0)
        rte_exit(EXIT_FAILURE, "no DPDK eth ports (did you bind the VF to vfio-pci?)\n");
    RTE_ETH_FOREACH_DEV(g_port)
        break; /* first available port */

    /* Ordered list of lcores; queue id = position in that list. */
    unsigned lcores[RTE_MAX_LCORE];
    unsigned nl = 0, lid;
    RTE_LCORE_FOREACH(lid) {
        lcores[nl] = lid;
        g_cfg[lid].queue_id = (uint16_t)nl;
        nl++;
    }
    if ((unsigned)g_nimg > nl)
        rte_exit(EXIT_FAILURE, "%d images but only %u lcores\n", g_nimg, nl);

    unsigned n_mbufs = nl * (NB_TXD + TX_BURST) * 2;
    if (n_mbufs < 16384)
        n_mbufs = 16384;
    g_mp = rte_pktmbuf_pool_create("MBUF_POOL", n_mbufs, MBUF_CACHE, 0,
                                   RTE_MBUF_DEFAULT_BUF_SIZE, rte_socket_id());
    if (!g_mp)
        rte_exit(EXIT_FAILURE, "mbuf pool create failed\n");

    port_init(g_port, (uint16_t)nl);

    /* Assign cores to images. Each core cycles the whole table with a distinct
     * phase offset so they don't all send the same frame at the same instant. */
    if (g_nimg == 1) {
        for (unsigned k = 0; k < nl; k++) {
            unsigned l = lcores[k];
            g_cfg[l].img = 0;
            g_cfg[l].active = 1;
        }
    } else {
        /* first N-1 images: one core each */
        for (int i = 0; i < g_nimg - 1; i++) {
            unsigned l = lcores[i];
            g_cfg[l].img = i;
            g_cfg[l].active = 1;
        }
        /* last image: all remaining cores */
        for (unsigned k = g_nimg - 1; k < nl; k++) {
            unsigned l = lcores[k];
            g_cfg[l].img = g_nimg - 1;
            g_cfg[l].active = 1;
        }
    }
    /* Spread phase offsets across the cores sharing an image. */
    for (unsigned k = 0; k < nl; k++) {
        unsigned l = lcores[k];
        struct frame_table *t =
            atomic_load_explicit(&g_img[g_cfg[l].img].tbl, memory_order_acquire);
        g_cfg[l].phase = t->n ? (uint32_t)((uint64_t)k * t->n / nl) % t->n : 0;
    }

    /* Start the file watcher only if at least one image is watched. */
    pthread_t watcher;
    int have_watch = 0;
    for (int i = 0; i < g_nimg; i++)
        have_watch |= g_img[i].watch;
    if (have_watch && pthread_create(&watcher, NULL, reloader, NULL) != 0)
        rte_exit(EXIT_FAILURE, "could not start reloader thread\n");

    rte_eal_mp_remote_launch(tx_loop, NULL, CALL_MAIN);
    rte_eal_mp_wait_lcore();

    if (have_watch)
        pthread_join(watcher, NULL);

    rte_eth_dev_stop(g_port);
    rte_eth_dev_close(g_port);
    rte_eal_cleanup();
    return 0;
}
