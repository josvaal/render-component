import { Component, input } from '@angular/core';

@Component({
  selector: 'app-dashboard-state',
  standalone: true,
  template:
    '<section class="dash-state"><h1>Dashboard State Component</h1><span class="state-id">{{ state().id }}</span></section>',
  styleUrls: ['./dashboard-state.component.scss'],
})
export class DashboardStateComponent {
  // NG0950 scenario: required signal input, provided by a parent in the real app.
  state = input.required<{ id: string }>();
}
