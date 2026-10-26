import ToastNotification from './components/organisms/ToastNotification.vue';

export * from './types';
export * from './utils';

export { ToastNotification };

export { useToastStore } from './stores/useToastStore';

export function opt<T>(v?: T) {
	return v ?? null;
}