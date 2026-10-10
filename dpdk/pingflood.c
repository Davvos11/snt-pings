/*
 * DPDK ICMPv6 pixel flooder for the Jinglepings canvas.
 *
 * This is a dumb, fast packet pump: it reads a file of pre-built, fully-formed
 * Ethernet + IPv6 + ICMPv6 frames (62 bytes each) produced by the Rust side
 * (`afxdp --dump frames.bin`) and blasts them out of an Intel VF via the iavf
 * PMD, one TX queue per lcore.
 *
 * All protocol logic (pixel encoding, headers, ICMPv6 checksum, and crucially
 * the source MAC = the VF's own MAC, required by Intel VF anti-spoofing) lives
 * in the Rust frame generator. This program never parses or builds a packet.
 *
 * Build:   (see Makefile)   needs DPDK installed + `pkg-config libdpdk`
 * Run:     sudo ./pingflood -l 0-3 -n 4 -a 0000:06:00.0 -- frames.bin
 *            -l   lcores to use (one TX queue each)
 *            -a   PCI address of the VF (after binding it to vfio-pci)
 *            --   everything after is this app's args: <frames file>
 */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>

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

static volatile int force_quit;

/* The full frame table, loaded once from the dump file and shared read-only. */
static uint8_t *g_frames;
static uint32_t g_nframes;

static struct rte_mempool *g_mp;
static uint16_t g_port;

/* Per-lcore work assignment: which TX queue, and which slice of the frame
 * table this core cycles through. Indexed by lcore id. */
struct lcore_cfg {
    uint16_t queue_id;
    uint32_t start;
    uint32_t count;
    int      active;
};
static struct lcore_cfg g_cfg[RTE_MAX_LCORE];

static void handle_signal(int sig)
{
    (void)sig;
    force_quit = 1;
}

/* Load the dump file (N * 62 bytes) into g_frames. */
static void load_frames(const char *path)
{
    FILE *f = fopen(path, "rb");
    if (!f)
        rte_exit(EXIT_FAILURE, "cannot open frames file '%s'\n", path);

    fseek(f, 0, SEEK_END);
    long sz = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (sz <= 0 || (sz % FRAME_LEN) != 0)
        rte_exit(EXIT_FAILURE, "frames file size %ld is not a multiple of %d\n",
                 sz, FRAME_LEN);

    g_frames = malloc(sz);
    if (!g_frames)
        rte_exit(EXIT_FAILURE, "out of memory loading frames\n");
    if (fread(g_frames, 1, sz, f) != (size_t)sz)
        rte_exit(EXIT_FAILURE, "short read on frames file\n");
    fclose(f);

    g_nframes = (uint32_t)(sz / FRAME_LEN);
    printf("loaded %u frames from %s\n", g_nframes, path);
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

    /* One RX queue is set up only because some PMDs require it to start the
     * device; we never poll it. */
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
 * repeat forever. Fresh mbufs each burst means the NIC owns and frees them
 * after TX — no in-flight reuse hazard. */
static int tx_loop(__rte_unused void *arg)
{
    unsigned lid = rte_lcore_id();
    struct lcore_cfg *cfg = &g_cfg[lid];
    if (!cfg->active || cfg->count == 0)
        return 0;

    const uint16_t q = cfg->queue_id;
    const uint32_t start = cfg->start;
    const uint32_t end = cfg->start + cfg->count;
    uint32_t idx = start;

    struct rte_mbuf *bufs[TX_BURST];
    uint64_t sent = 0;
    const uint64_t hz = rte_get_tsc_hz();
    uint64_t last = rte_get_tsc_cycles();

    printf("lcore %u -> TX queue %u, frames [%u, %u)\n", lid, q, start, end);

    while (!force_quit) {
        if (rte_pktmbuf_alloc_bulk(g_mp, bufs, TX_BURST) != 0)
            continue; /* pool momentarily drained; retry */

        for (int i = 0; i < TX_BURST; i++) {
            uint8_t *d = rte_pktmbuf_mtod(bufs[i], uint8_t *);
            rte_memcpy(d, g_frames + (size_t)idx * FRAME_LEN, FRAME_LEN);
            bufs[i]->data_len = FRAME_LEN;
            bufs[i]->pkt_len = FRAME_LEN;
            if (++idx >= end)
                idx = start;
        }

        /* Transmit the whole burst, retrying the remainder the NIC didn't take. */
        uint16_t nb = 0;
        while (nb < TX_BURST) {
            uint16_t n = rte_eth_tx_burst(g_port, q, &bufs[nb], TX_BURST - nb);
            nb += n;
            if (n == 0 && force_quit)
                break;
        }
        sent += nb;
        /* Free any left unsent (only possible on shutdown). */
        for (uint16_t i = nb; i < TX_BURST; i++)
            rte_pktmbuf_free(bufs[i]);

        uint64_t now = rte_get_tsc_cycles();
        if (now - last >= hz) {
            printf("lcore %u q%u: %.2f Mpps\n", lid, q,
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
                 "usage: %s [EAL args] -- <frames.bin>\n", argv[0]);
    load_frames(argv[1]);

    force_quit = 0;
    signal(SIGINT, handle_signal);
    signal(SIGTERM, handle_signal);

    if (rte_eth_dev_count_avail() == 0)
        rte_exit(EXIT_FAILURE, "no DPDK eth ports (did you bind the VF to vfio-pci?)\n");
    RTE_ETH_FOREACH_DEV(g_port)
        break; /* use the first available port */

    unsigned nb_lcores = rte_lcore_count();

    /* One big shared mbuf pool, sized to comfortably cover all in-flight TX. */
    unsigned n_mbufs = nb_lcores * (NB_TXD + TX_BURST) * 2;
    if (n_mbufs < 16384)
        n_mbufs = 16384;
    g_mp = rte_pktmbuf_pool_create("MBUF_POOL", n_mbufs, MBUF_CACHE, 0,
                                   RTE_MBUF_DEFAULT_BUF_SIZE, rte_socket_id());
    if (!g_mp)
        rte_exit(EXIT_FAILURE, "mbuf pool create failed\n");

    port_init(g_port, (uint16_t)nb_lcores);

    /* Partition the frame table across lcores: each core repaints its own slice,
     * so together they cover every pixel and waste no capacity on duplicates. */
    uint32_t chunk = g_nframes / nb_lcores;
    unsigned w = 0;
    unsigned lid;
    RTE_LCORE_FOREACH(lid) {
        uint32_t s = w * chunk;
        uint32_t c = (w == nb_lcores - 1) ? (g_nframes - s) : chunk;
        g_cfg[lid].queue_id = (uint16_t)w;
        g_cfg[lid].start = s;
        g_cfg[lid].count = c;
        g_cfg[lid].active = 1;
        w++;
    }

    rte_eal_mp_remote_launch(tx_loop, NULL, CALL_MAIN);
    rte_eal_mp_wait_lcore();

    rte_eth_dev_stop(g_port);
    rte_eth_dev_close(g_port);
    rte_eal_cleanup();
    return 0;
}
