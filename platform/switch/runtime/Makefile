DEVKITPRO ?= /opt/devkitpro
DEVKITA64 ?= $(DEVKITPRO)/devkitA64

TARGET := siglus_switch
BUILD := build
ROMFS ?= romfs
CC := $(DEVKITA64)/bin/aarch64-none-elf-gcc
ELF2NRO := $(DEVKITPRO)/tools/bin/elf2nro
NACPTOOL := $(DEVKITPRO)/tools/bin/nacptool
UAM := $(DEVKITPRO)/tools/bin/uam

CPPFLAGS := -I$(DEVKITPRO)/libnx/include
CFLAGS := -std=gnu17 -O2 -g -Wall -Wextra -ffunction-sections -fdata-sections \
	-march=armv8-a+crc+crypto -mtune=cortex-a57 -mtp=soft -fPIE $(CPPFLAGS)
LDFLAGS := -specs=$(DEVKITPRO)/libnx/switch.specs -g \
	-march=armv8-a+crc+crypto -mtune=cortex-a57 -mtp=soft -fPIE \
	-Wl,--gc-sections -Wl,-Map,$(TARGET).map \
	-Wl,--wrap=nvMapCreate -Wl,--wrap=nvAddressSpaceMap -L$(DEVKITPRO)/libnx/lib
LDLIBS := -ldeko3dd -lnx -lm
RUST_LIB := ../../../target/aarch64-switch/release/libsiglus_scene_vm.a

OBJECTS := $(BUILD)/main.o
SHADERS := $(ROMFS)/shaders/siglus_vsh.dksh $(ROMFS)/shaders/siglus_fsh.dksh

.PHONY: all clean
all: $(TARGET).nro

$(RUST_LIB):
	cd ../../.. && RUSTFLAGS='--cfg getrandom_backend="custom"' rustup run nightly-2026-01-20 cargo build -p siglus_scene_vm --lib --release --target platform/switch/rust/aarch64-switch.json -Zbuild-std=std,panic_abort

$(BUILD) $(ROMFS)/shaders:
	mkdir -p $@

$(BUILD)/main.o: source/main.c | $(BUILD)
	$(CC) $(CFLAGS) -MMD -MP -c $< -o $@

$(ROMFS)/shaders/siglus_vsh.dksh: source/siglus_vsh.glsl | $(ROMFS)/shaders
	$(UAM) --stage=vert --out=$@ $<

$(ROMFS)/shaders/siglus_fsh.dksh: source/siglus_fsh.glsl | $(ROMFS)/shaders
	$(UAM) --stage=frag --out=$@ $<

$(TARGET).elf: $(OBJECTS) $(RUST_LIB)
	$(CC) $(LDFLAGS) $(OBJECTS) $(RUST_LIB) $(LDLIBS) -o $@

$(TARGET).nacp:
	$(NACPTOOL) --create "Siglus Switch" "siglus_rs" "0.1.0" $@ --titleid=0100A11C5EED0001

$(TARGET).nro: $(TARGET).elf $(TARGET).nacp $(SHADERS)
	$(ELF2NRO) $< $@ --nacp=$(TARGET).nacp --romfsdir=$(ROMFS)

clean:
	rm -rf $(BUILD) $(ROMFS) $(TARGET).elf $(TARGET).map $(TARGET).nacp $(TARGET).nro

-include $(OBJECTS:.o=.d)
