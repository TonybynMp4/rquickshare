import { defineStore } from "pinia";
import { ref } from "vue";
import { Toast, ToastType } from "../types";

export const useToastStore = defineStore("toasts", () => {
	const toasts = ref<Toast[]>([]);
	// A counter, not Date.now(): two toasts in the same millisecond got the
	// same id, so they shared a v-for key and were dismissed together.
	let nextId = 0;

	function addToast(message: string, type: ToastType) {
		const toast = {
			id: nextId++,
			message,
			type,
		};
		toasts.value.push(toast);
		setTimeout(() => removeToast(toast.id), 3000);
	}

	function removeToast(id: number) {
		toasts.value = toasts.value.filter((toast) => toast.id !== id);
	}

	return { toasts, addToast, removeToast };
});