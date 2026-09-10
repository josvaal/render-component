import { Component } from '@angular/core';
import { PhotoCardComponent } from './photo-card/photo-card.component';

@Component({
  selector: 'app-gallery',
  standalone: true,
  imports: [PhotoCardComponent],
  templateUrl: './gallery.component.html',
  styleUrls: ['./gallery.component.scss'],
})
export class GalleryComponent {}
