<script setup lang="ts">
import { PropType, computed, ref } from 'vue';

const props = defineProps({
	files: {
		type: Array as PropType<string[]>,
		required: true
	},
	// Rows shown before the rest folds behind "Show N more". A large transfer
	// would otherwise push the card's buttons off screen.
	max: {
		type: Number,
		default: 4
	}
});

const expanded = ref(false);

// Folding a single row saves no space, so only fold when two or more would hide.
const folds = computed(() => props.files.length > props.max + 1);
const shown = computed(() => folds.value && !expanded.value ? props.files.slice(0, props.max) : props.files);
</script>

<template>
	<ul class="flex flex-col gap-1 mt-2">
		<li v-for="(f, i) in shown" :key="i" class="flex flex-row items-center gap-2 min-w-0" :title="f">
			<svg
				xmlns="http://www.w3.org/2000/svg" fill="currentColor" height="16" viewBox="0 -960 960 960"
				width="16" class="shrink-0 opacity-70" aria-hidden="true">
				<!-- eslint-disable-next-line -->
				<path d="M240-80q-33 0-56.5-23.5T160-160v-640q0-33 23.5-56.5T240-880h320l240 240v480q0 33-23.5 56.5T720-80H240Zm280-520v-200H240v640h480v-440H520ZM240-800v200-200 640-640Z" />
			</svg>
			<span class="overflow-hidden whitespace-nowrap text-ellipsis">{{ f }}</span>
		</li>
		<li v-if="folds">
			<button
				type="button" class="text-xs underline opacity-70 transition duration-150 ease-in-out hover:opacity-100"
				@click.stop="expanded = !expanded">
				{{ expanded ? 'Show less' : `Show ${files.length - max} more` }}
			</button>
		</li>
	</ul>
</template>
