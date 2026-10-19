// `/analytics` opens the first section, keeping the scope and range.
import { redirect } from '@sveltejs/kit';
import type { PageLoad } from './$types';

export const load: PageLoad = ({ url }) => {
	redirect(307, `/analytics/overview${url.search}`);
};
