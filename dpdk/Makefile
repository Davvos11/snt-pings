# Build the DPDK pixel flooder against an installed DPDK (pkg-config libdpdk).
#
# If pkg-config can't find libdpdk, point it at DPDK's pkgconfig dir, e.g.:
#   export PKG_CONFIG_PATH=/usr/local/lib/x86_64-linux-gnu/pkgconfig
#
# Then just: make

APP = pingflood
SRCS = pingflood.c

PKGCONF ?= pkg-config

ifeq ($(shell $(PKGCONF) --exists libdpdk && echo y),)
$(error "libdpdk not found by pkg-config. Set PKG_CONFIG_PATH to DPDK's pkgconfig dir.")
endif

CFLAGS += -O3 -Wall -Wextra $(shell $(PKGCONF) --cflags libdpdk)
LDFLAGS += $(shell $(PKGCONF) --libs libdpdk)

$(APP): $(SRCS)
	$(CC) $(CFLAGS) $(SRCS) -o $(APP) $(LDFLAGS)

.PHONY: clean
clean:
	rm -f $(APP)
