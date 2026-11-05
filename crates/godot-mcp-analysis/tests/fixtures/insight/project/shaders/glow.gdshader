shader_type canvas_item;

#include "res://shaders/common.gdshaderinc"

void fragment() {
	COLOR = glow(COLOR);
}
