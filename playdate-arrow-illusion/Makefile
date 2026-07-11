# Playdate C build. Requires the Playdate SDK installed and ~/.Playdate/config
# pointing SDKRoot at it (the SDK installer sets this up automatically).
#
#   make          build the simulator .pdx
#   make device   build for hardware (needs the ARM GCC toolchain)
#   make run      build and open in the Playdate Simulator

HEAP_SIZE      = 8388208
STACK_SIZE     = 61800

PRODUCT = ArrowIllusion.pdx

# Locate the SDK from ~/.Playdate/config.
SDK = $(shell egrep '^\s*SDKRoot' $(HOME)/.Playdate/config | head -n 1 | cut -c9-)

VPATH += src

# List C source files here.
SRC = src/main.c

# Compiler / linker extras.
UDEFS =
UADEFS =
UINCDIR =
ULIBDIR =
ULIBS =

include $(SDK)/C_API/buildsupport/common.mk
